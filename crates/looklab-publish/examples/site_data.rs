// SPDX-License-Identifier: AGPL-3.0-or-later
//! Generates the sample static site: three synthetic Look fits, canonical scenes,
//! bodies and an audit log, written to `site/data` (or the first argument).
#![allow(clippy::disallowed_methods)]

use std::error::Error;
use std::fs;
use std::path::PathBuf;

use looklab_fit::synth::{SynthConfig, SyntheticDataset, dataset};
use looklab_fit::{FitOptions, fit};
use looklab_model::file::{
    BodyProvenance, LookIdentity, LookKind, Provenance, RootProvenance, WbPolicy,
};
use looklab_model::{Density, LookDocument, ProbeCovariance, UncertaintyData};
use looklab_publish::{
    ArtifactBlob, ArtifactHash, ArtifactName, AuditEntry, AuditEvent, AuditLog, BodyExtras,
    BodyPage, BodyUse, CredentialRef, CredentialTier, Decision, GateCheck, GateSummary,
    InputTransformPage, LicensePage, LookPage, LookRef, LookStatus, LookVersionPage, ProposalRef,
    ProvenanceSummary, RealizationNote, Role, Scene, SceneName, SiteData, SitePath, Timestamp,
    TransformSource, VersionNotes, VersionRef, WeakSpot, bake_preview_lut, write_site,
};
use looklab_schema::{validate_body, validate_control_schema};

const SIZE: u32 = 1414;
const CREATED: &str = "2026-11-04T00:00:00Z";

fn main() -> Result<(), Box<dyn Error>> {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "site".to_owned()));
    fs::create_dir_all(&out)?;

    let body = source_body()?;
    let mut bodies = vec![body];
    if let Some(mut second) = bodies[0].clone().into_second_body() {
        second.mapping_status = looklab_publish::MappingStatus::Provisional;
        second.input_transform = Some(InputTransformPage {
            version: 1,
            source: TransformSource::Chart,
            anchor_count: 3,
            tier: CredentialTier::New,
        });
        second.realization_notes = vec![RealizationNote {
            epoch: None,
            text: "Provisional mapping: no donated fixture yet.".to_owned(),
        }];
        bodies.push(second);
    }

    let looks = synthetic_looks()?;
    let mut audit = vec![
        AuditEntry {
            seq: 1,
            ts: Timestamp::new(CREATED)?,
            event: AuditEvent::Grant {
                subject: CredentialRef::new("cred-7f21")?,
                role: Role::Moderator,
                granted_by: CredentialRef::new("cli")?,
                reason: "moderation queue bootstrap".to_owned(),
            },
        },
        AuditEntry {
            seq: 2,
            ts: Timestamp::new(CREATED)?,
            event: AuditEvent::Endorsement {
                issuer: CredentialRef::new("cred-19aa")?,
                subject: CredentialRef::new("cred-7f21")?,
                reason: "chart sessions reviewed".to_owned(),
            },
        },
        AuditEntry {
            seq: 3,
            ts: Timestamp::new(CREATED)?,
            event: AuditEvent::ModerationDecision {
                proposal: ProposalRef::new("proposal-4")?,
                moderator: CredentialRef::new("cred-7f21")?,
                decision: Decision::Approve,
                reason: "round-trip report matches the submission".to_owned(),
            },
        },
    ];
    let mut seq = audit.len() as u64;
    for look in &looks {
        for version in &look.versions {
            let manifest = version.manifest.to_json()?;
            seq += 1;
            audit.push(AuditEntry {
                seq,
                ts: Timestamp::new(CREATED)?,
                event: AuditEvent::Release {
                    look: LookRef::new(look.slug.as_str())?,
                    version: VersionRef::new(version.version().as_str())?,
                    manifest_sha256: ArtifactHash::of(&manifest),
                },
            });
        }
    }

    let site = SiteData {
        bodies,
        looks,
        audit_log: AuditLog::new(audit),
        scenes: canonical_scenes(),
    };

    let mut sink = |path: &SitePath, bytes: &[u8]| -> std::io::Result<()> {
        let full = out.join(path.as_str());
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(full, bytes)
    };
    let root = write_site(&mut sink, &site)?;
    fs::write(out.join("site-root.txt"), root.as_str())?;
    println!(
        "wrote {}/data ({} bytes of index)",
        out.display(),
        fs::read(out.join("data/index.json"))
            .map(|bytes| bytes.len())
            .unwrap_or(0)
    );
    Ok(())
}

fn source_body() -> Result<BodyPage, Box<dyn Error>> {
    let body = validate_body(include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml"))
        .map_err(|errors| format!("body: {errors:?}"))?;
    let schema = validate_control_schema(include_str!("../../../data/bodies/fujifilm-x-t5.yaml"))
        .map_err(|errors| format!("schema: {errors:?}"))?;
    let extras = BodyExtras {
        input_transform: Some(InputTransformPage {
            version: 7,
            source: TransformSource::Chart,
            anchor_count: 12,
            tier: CredentialTier::Endorsed,
        }),
        realization_notes: vec![
            RealizationNote {
                epoch: Some(1),
                text: "Renders about 0.5 EV darker in DR200 than the reference body.".to_owned(),
            },
            RealizationNote {
                epoch: Some(2),
                text: "Same tone response as epoch 1; color chrome blue differs in the shadows."
                    .to_owned(),
            },
        ],
    };
    Ok(BodyPage::from_schema(&body, &schema, &extras)?)
}

fn synthetic_looks() -> Result<Vec<LookPage>, Box<dyn Error>> {
    let specs = [
        (
            11_u64,
            "sunlit-negative",
            "Sunlit Negative",
            "Warm negative film: lifted shadows, gentle highlight rolloff.",
            0.5_f64,
            "deep shadows",
            "Shadow separation is weaker than the reference body.",
            None,
        ),
        (
            22,
            "cold-steel",
            "Cold Steel",
            "Cool, high-contrast look with a tightened chroma plane.",
            -0.4,
            "skin midtones",
            "Midtone skin drifts slightly green at low chroma.",
            None,
        ),
        (
            33,
            "neutral-studio",
            "Neutral Studio",
            "Near-colorimetric studio look for product and copy work.",
            0.0,
            "saturated reds",
            "Red highlights reach the soft clip earlier than the source scene.",
            Some("This version was produced by a revert to an earlier fit; the excluded credential is listed in the audit log.".to_owned()),
        ),
    ];
    let mut looks = Vec::new();
    for (seed, slug, name, description, lift, region, note, notice) in specs {
        let config = SynthConfig {
            seed,
            images: 2,
            samples_per_image: 40,
            ..SynthConfig::default()
        };
        let data = dataset(&config);
        let result = fit(
            &data.images,
            &FitOptions {
                seed,
                max_sweeps: 2,
                max_samples_per_image: 80,
                hessian_samples: 200,
                sweep_budget: 20_000.0,
                run_gcv: false,
                fit_residual: false,
                ..FitOptions::default()
            },
        )
        .unwrap_or_else(|_| minimal_result(&data));
        let mut document = LookDocument::from_look(
            LookIdentity::new(slug, "1.0.0", LookKind::NativeFamily, WbPolicy::Adaptive)?,
            result.look.clone(),
        )?;
        let uncertainty = uncertainty(&result)?;
        document = document.with_uncertainty(uncertainty)?;
        document.file.provenance = provenance(&data, &result)?;
        let look_bytes = document.encode()?;
        let lut = bake_preview_lut(&document);

        let slug = looklab_model::file::Slug::new(slug)?;
        let version = LookVersionPage::new(
            slug.clone(),
            looklab_model::file::LookVersion::new("1.0.0")?,
            LookStatus::Published,
            gates()?,
            provenance_summary(&data, lift),
            vec![
                ArtifactBlob::new(ArtifactName::new("look.looklab")?, look_bytes),
                ArtifactBlob::new(ArtifactName::new("preview.lut33")?, lut),
            ],
            VersionNotes {
                changelog: vec![format!("initial synthetic fit (seed {seed})")],
                weak_spots: vec![WeakSpot {
                    region: region.to_owned(),
                    note: note.to_owned(),
                    bounty: Some("bounty-1".to_owned()),
                }],
                notice,
            },
        )?;
        looks.push(LookPage {
            slug,
            name: name.to_owned(),
            kind: LookKind::NativeFamily,
            wb_policy: WbPolicy::Adaptive,
            description: description.to_owned(),
            aliases: Vec::new(),
            license: Some(LicensePage {
                declaration: "Project-owned synthetic data".to_owned(),
                attribution: "looklab".to_owned(),
            }),
            versions: vec![version],
        });
    }
    Ok(looks)
}

fn minimal_result(data: &SyntheticDataset) -> looklab_fit::FitResult {
    looklab_fit::FitResult {
        look: data.truth.clone(),
        images: Vec::new(),
        realization: looklab_fit::Realization {
            clip_k: data.clip_k,
            exposure_offsets: Vec::new(),
        },
        lambdas: looklab_fit::StageLambdas::default(),
        probes: looklab_fit::ProbeUncertainty {
            probes: Vec::new(),
            ids: Vec::new(),
            supported_fraction: 0.0,
        },
        density: looklab_fit::Density {
            s_bins: 0,
            hue_bins: 0,
            c_bins: 0,
            counts: Vec::new(),
            total: 0.0,
            floor: 0.0,
        },
        image_stats: Vec::new(),
        diagnostics: looklab_fit::Diagnostics::default(),
        control_response: None,
        excluded: Vec::new(),
    }
}

fn uncertainty(result: &looklab_fit::FitResult) -> Result<UncertaintyData, Box<dyn Error>> {
    let probes = result.probes.probes.len();
    if probes == 0 {
        return Ok(UncertaintyData::default());
    }
    let mut covariance = Vec::with_capacity(probes * 9);
    let mut density = Vec::with_capacity(probes);
    for probe in &result.probes.probes {
        for row in &probe.covariance {
            covariance.extend_from_slice(row);
        }
        density.push(probe.density);
    }
    Ok(UncertaintyData {
        probe_cov: Some(ProbeCovariance::new(probes, covariance)?),
        density: Some(Density::new(density)?),
    })
}

fn provenance(
    data: &SyntheticDataset,
    result: &looklab_fit::FitResult,
) -> Result<Provenance, Box<dyn Error>> {
    let prep_versions = data
        .images
        .iter()
        .map(|image| image.samples.header().prep_version.get())
        .collect::<Vec<_>>();
    Ok(Provenance {
        bodies: vec![BodyProvenance {
            body: "fujifilm/x-t5".to_owned(),
            epoch_map_version: 4,
            epochs: vec![1],
            input_transform_version: 7,
            realization_params_version: 3,
            control_response_version: 1,
            clip_k: result.realization.clip_k,
            images: data.images.len() as u32,
            credentials: 1,
        }],
        root: RootProvenance {
            admissions: Vec::new(),
            anchor_version: "1.0.0".to_owned(),
        },
        community_snapshot_version: 1,
        prep_versions,
        created: CREATED.to_owned(),
        software: "looklab-publish site_data".to_owned(),
    })
}

fn provenance_summary(data: &SyntheticDataset, lift: f64) -> ProvenanceSummary {
    ProvenanceSummary {
        bodies: vec![BodyUse {
            body: "fujifilm/x-t5".to_owned(),
            epochs: vec![1],
            images: data.images.len() as u32,
            credentials: 1,
        }],
        root_admissions: Vec::new(),
        anchor_version: "1.0.0".to_owned(),
        community_snapshot_version: 1,
        prep_versions: vec![1],
        created: CREATED.to_owned(),
        software: format!("looklab-publish site_data (lift {lift:+.1})"),
    }
}

fn gates() -> Result<GateSummary, Box<dyn Error>> {
    Ok(GateSummary {
        checks: vec![
            GateCheck {
                name: "identity tolerance".to_owned(),
                passed: true,
                detail: Some("chromatic probes within 0.05 J'".to_owned()),
            },
            GateCheck {
                name: "tone monotone".to_owned(),
                passed: true,
                detail: None,
            },
            GateCheck {
                name: "held-out prediction".to_owned(),
                passed: true,
                detail: Some("median ΔE 0.41".to_owned()),
            },
            GateCheck {
                name: "artifact manifest".to_owned(),
                passed: true,
                detail: Some("every artifact listed by SHA-256".to_owned()),
            },
        ],
    })
}

fn canonical_scenes() -> Vec<Scene> {
    vec![gradient_scene(), hue_wheel_scene(), patches_scene()]
}

fn gradient_scene() -> Scene {
    let colors = [
        [0.82, 0.18, 0.16],
        [0.88, 0.55, 0.18],
        [0.86, 0.82, 0.24],
        [0.28, 0.62, 0.30],
        [0.22, 0.44, 0.78],
        [0.48, 0.28, 0.66],
    ];
    let mut xyz = Vec::with_capacity((SIZE * SIZE) as usize);
    for y in 0..SIZE {
        let v = y as f64 / (SIZE - 1) as f64;
        for x in 0..SIZE {
            let u = x as f64 / (SIZE - 1) as f64;
            let encoded = if u < 0.5 {
                let level = 0.002 + 1.0 * (u / 0.5);
                [level, level, level]
            } else {
                let band =
                    (((u - 0.5) * colors.len() as f64).floor() as usize).min(colors.len() - 1);
                let shade = 0.10 + 0.90 * (1.0 - v);
                [
                    colors[band][0] * shade,
                    colors[band][1] * shade,
                    colors[band][2] * shade,
                ]
            };
            xyz.push(encoded_xyz(encoded));
        }
    }
    Scene::new(
        SceneName::new("gradient").expect("scene name"),
        SIZE,
        SIZE,
        xyz,
    )
    .expect("scene")
}

fn hue_wheel_scene() -> Scene {
    let mut xyz = Vec::with_capacity((SIZE * SIZE) as usize);
    let center = (SIZE - 1) as f64 / 2.0;
    let radius = center;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f64 - center;
            let dy = y as f64 - center;
            let distance = (dx * dx + dy * dy).sqrt() / radius;
            let angle = libm::atan2(dy, dx);
            let hue = (angle / (2.0 * core::f64::consts::PI)).rem_euclid(1.0);
            let saturation = distance.clamp(0.0, 1.0);
            let value = 1.0 - 0.55 * distance.clamp(0.0, 1.0);
            let encoded = hsv(hue, saturation, value);
            xyz.push(encoded_xyz(encoded));
        }
    }
    Scene::new(
        SceneName::new("hue-wheel").expect("scene name"),
        SIZE,
        SIZE,
        xyz,
    )
    .expect("scene")
}

fn patches_scene() -> Scene {
    let patches = [
        ([0.91, 0.76, 0.66], "light skin"),
        ([0.55, 0.36, 0.26], "deep skin"),
        ([0.22, 0.42, 0.16], "foliage"),
        ([0.44, 0.66, 0.92], "sky"),
        ([0.18, 0.18, 0.18], "neutral 18%"),
        ([0.92, 0.92, 0.92], "white"),
        ([0.04, 0.04, 0.04], "black"),
        ([0.78, 0.12, 0.10], "red"),
        ([0.16, 0.60, 0.22], "green"),
        ([0.12, 0.32, 0.78], "blue"),
        ([0.96, 0.72, 0.28], "highlight"),
        ([0.10, 0.09, 0.12], "shadow"),
    ];
    let cols = 4_u32;
    let rows = 3_u32;
    let mut xyz = Vec::with_capacity((SIZE * SIZE) as usize);
    for y in 0..SIZE {
        let row = (y * rows / SIZE).min(rows - 1) as usize;
        let v = (y % (SIZE / rows)) as f64 / (SIZE / rows) as f64;
        for x in 0..SIZE {
            let col = (x * cols / SIZE).min(cols - 1) as usize;
            let patch = patches[(row * cols as usize + col).min(patches.len() - 1)].0;
            let shade = 0.72 + 0.28 * v;
            xyz.push(encoded_xyz([
                patch[0] * shade,
                patch[1] * shade,
                patch[2] * shade,
            ]));
        }
    }
    Scene::new(
        SceneName::new("patches").expect("scene name"),
        SIZE,
        SIZE,
        xyz,
    )
    .expect("scene")
}

fn encoded_xyz(encoded: [f64; 3]) -> [f32; 3] {
    let xyz = looklab_color::RgbSpace::Srgb
        .rgb_encoded_to_xyz(looklab_color::Rgb::new(encoded[0], encoded[1], encoded[2]));
    [xyz.x as f32, xyz.y as f32, xyz.z as f32]
}

fn hsv(h: f64, s: f64, v: f64) -> [f64; 3] {
    let h6 = h * 6.0;
    let i = h6.floor();
    let f = h6 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match (i as i64).rem_euclid(6) {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

trait SecondBody {
    fn into_second_body(self) -> Option<BodyPage>;
}

impl SecondBody for BodyPage {
    fn into_second_body(mut self) -> Option<BodyPage> {
        let id = looklab_schema::BodyId::new("fujifilm/x-m1").ok()?;
        self.id = id;
        self.make = "Fujifilm".to_owned();
        self.model = "X-M1".to_owned();
        self.exif_aliases = vec!["X-M1".to_owned()];
        self.status = looklab_publish::BodyStatus::Active;
        Some(self)
    }
}
