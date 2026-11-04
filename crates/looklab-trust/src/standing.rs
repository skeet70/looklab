// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tiers, scoped standing and endorsement scope ([docs/05 §3]).

use looklab_upload::{BodyId, LookSlug};
use serde::{Deserialize, Serialize};

use crate::params::TrustParams;
use crate::reputation::Reputation;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    New,
    Endorsed,
    Seeder,
}

impl Tier {
    pub fn starting_reputation(self, params: &TrustParams) -> Reputation {
        let value = match self {
            Tier::New | Tier::Endorsed => params.reputation_new_start,
            Tier::Seeder => params.reputation_seeder_start,
        };
        Reputation::saturating(value)
    }
}

/// The tier that applies to one `(look, body)` after the scope check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Standing {
    New,
    Endorsed,
    Seeder,
}

impl Standing {
    /// Outside its scope an endorsed or seeder credential counts as `new`.
    pub fn resolve(tier: Tier, scope: Option<&Scope>, look: &LookSlug, body: &BodyId) -> Self {
        let covered = match scope {
            Some(scope) => scope.covers(look, body),
            None => true,
        };
        match (tier, covered) {
            (Tier::New, _) => Standing::New,
            (Tier::Endorsed | Tier::Seeder, false) => Standing::New,
            (Tier::Endorsed, true) => Standing::Endorsed,
            (Tier::Seeder, true) => Standing::Seeder,
        }
    }

    pub fn tier(self) -> Tier {
        match self {
            Standing::New => Tier::New,
            Standing::Endorsed => Tier::Endorsed,
            Standing::Seeder => Tier::Seeder,
        }
    }

    pub fn factor(self, params: &TrustParams) -> f64 {
        match self {
            Standing::New => params.tier_factor_new,
            Standing::Endorsed | Standing::Seeder => params.tier_factor_trusted,
        }
    }
}

/// One scope rule: an image is in scope when its body and Look match every field that
/// is `Some`. `None` is the wildcard.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ScopeRule {
    pub body: Option<BodyId>,
    pub look: Option<LookSlug>,
}

impl ScopeRule {
    pub fn new(body: Option<BodyId>, look: Option<LookSlug>) -> Self {
        Self { body, look }
    }

    pub fn body_all_looks(body: BodyId) -> Self {
        Self {
            body: Some(body),
            look: None,
        }
    }

    pub fn any_body(look: LookSlug) -> Self {
        Self {
            body: None,
            look: Some(look),
        }
    }

    pub fn matches(&self, look: &LookSlug, body: &BodyId) -> bool {
        self.body.as_ref().is_none_or(|value| value == body)
            && self.look.as_ref().is_none_or(|value| value == look)
    }

    /// Whether every pair matched by `other` is matched by this rule.
    fn covers(&self, other: &ScopeRule) -> bool {
        self.body
            .as_ref()
            .is_none_or(|body| other.body.as_ref() == Some(body))
            && self
                .look
                .as_ref()
                .is_none_or(|look| other.look.as_ref() == Some(look))
    }

    pub fn render(&self) -> String {
        match (&self.body, &self.look) {
            (Some(body), None) => format!("{} · all looks", render_body(body)),
            (None, Some(look)) => format!("{} · any body", render_look(look)),
            (Some(body), Some(look)) => {
                format!("{} · {}", render_body(body), render_look(look))
            }
            (None, None) => "any body · any look".to_owned(),
        }
    }
}

/// An endorsement or grant scope: a list of rules. An image `(body, look)` is in scope
/// if any rule matches it.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Scope {
    pub rules: Vec<ScopeRule>,
}

impl Scope {
    pub fn new(rules: impl IntoIterator<Item = ScopeRule>) -> Self {
        let mut rules: Vec<ScopeRule> = rules.into_iter().collect();
        rules.sort();
        rules.dedup();
        Self { rules }
    }

    /// Migration from the old OR form: `(b, *)` for each body, `(*, l)` for each Look.
    pub fn from_legacy(
        bodies: impl IntoIterator<Item = BodyId>,
        looks: impl IntoIterator<Item = LookSlug>,
    ) -> Self {
        Self::new(
            bodies
                .into_iter()
                .map(ScopeRule::body_all_looks)
                .chain(looks.into_iter().map(ScopeRule::any_body)),
        )
    }

    pub fn covers(&self, look: &LookSlug, body: &BodyId) -> bool {
        self.rules.iter().any(|rule| rule.matches(look, body))
    }

    /// Body parameters are scoped by a rule that names the body.
    pub fn covers_body(&self, body: &BodyId) -> bool {
        self.rules
            .iter()
            .any(|rule| rule.body.as_ref() == Some(body))
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// An empty issuer scope cannot authorize a scoped endorsement.
    pub fn contains(&self, other: &Scope) -> bool {
        if other.is_empty() || self.is_empty() {
            return false;
        }
        other
            .rules
            .iter()
            .all(|rule| self.rules.iter().any(|mine| mine.covers(rule)))
    }

    pub fn render(&self) -> String {
        self.rules
            .iter()
            .map(ScopeRule::render)
            .collect::<Vec<_>>()
            .join("; ")
    }
}

fn render_body(body: &BodyId) -> String {
    let (make, model) = body.as_str().split_once('/').unwrap_or((body.as_str(), ""));
    if model.is_empty() {
        title_case_hyphenated(make)
    } else {
        format!(
            "{} {}",
            title_case_hyphenated(make),
            title_case_hyphenated(model)
        )
    }
}

fn render_look(look: &LookSlug) -> String {
    look.as_str()
        .split('-')
        .map(title_case)
        .collect::<Vec<_>>()
        .join(" ")
}

fn title_case_hyphenated(value: &str) -> String {
    value
        .split('-')
        .map(title_case)
        .collect::<Vec<_>>()
        .join("-")
}

fn title_case(value: &str) -> String {
    let mut characters = value.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => String::new(),
    }
}
