// SPDX-License-Identifier: AGPL-3.0-or-later
//! Honest duplicate merge: one effective contributor, one capture tuple, one weight.

use std::collections::BTreeMap;

use looklab_trust::{
    CaptureTuple, EffectiveContributorId, SerialLink, resolve_effective_contributors,
};
use sqlx::SqlitePool;

use crate::db;
use crate::error::Result;
use crate::meta;

type GroupKey = (EffectiveContributorId, String, String, CaptureTuple);

pub async fn run(pool: &SqlitePool) -> Result<()> {
    let contributions = db::all_contributions(pool).await?;
    let mut by_id = BTreeMap::new();
    for contribution in &contributions {
        by_id.insert(contribution.id.as_str().to_owned(), contribution.clone());
    }
    let ids: Vec<_> = contributions
        .iter()
        .map(|contribution| contribution.id.clone())
        .collect();
    let images = db::images_for_contributions(pool, &ids).await?;

    let links: Vec<SerialLink> = db::serial_links(pool)
        .await?
        .into_iter()
        .map(|(credential, hmac)| {
            Ok(SerialLink {
                credential: looklab_server::admin::trust_id(&credential)?,
                hmac: looklab_meta::SerialHmac::from_bytes(hmac),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let credentials = contributions
        .iter()
        .map(|contribution| looklab_server::admin::trust_id(&contribution.credential))
        .collect::<core::result::Result<Vec<_>, _>>()?;
    let effective = resolve_effective_contributors(credentials, &links);

    let mut groups: BTreeMap<GroupKey, Vec<(i64, db::ImageId)>> = BTreeMap::new();
    for image in images {
        let Some(contribution) = by_id.get(image.contribution.as_str()) else {
            continue;
        };
        let Some(tuple) = capture(&image.meta_json) else {
            continue;
        };
        let credential = looklab_server::admin::trust_id(&contribution.credential)?;
        let contributor = effective
            .get(&credential)
            .copied()
            .unwrap_or_else(|| EffectiveContributorId::from(credential));
        let key = (
            contributor,
            contribution.body.as_str().to_owned(),
            contribution.sim_key.clone(),
            tuple,
        );
        groups
            .entry(key)
            .or_default()
            .push((contribution.created_at, image.id));
    }

    db::clear_duplicates(pool).await?;
    for mut group in groups.into_values() {
        group.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.as_str().cmp(&right.1.as_str()))
        });
        let Some((_, representative)) = group.first() else {
            continue;
        };
        for (_, image) in &group[1..] {
            db::mark_duplicate(pool, image, representative).await?;
        }
    }
    Ok(())
}

fn capture(meta_json: &str) -> Option<CaptureTuple> {
    meta::capture_tuple(meta_json).ok()
}
