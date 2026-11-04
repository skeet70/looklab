// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeMap;

use looklab_model::ProbeSetVersion;

use crate::emphasis::Emphasis;
use crate::error::{MatchError, Result};

pub const PERMALINK_FORMAT: &str = "looklab-match/1";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PermalinkSources {
    pub target_look: Option<String>,
    pub base_look: Option<String>,
    pub body: Option<String>,
    pub crm: Option<String>,
    pub transform: Option<String>,
    pub covariance: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permalink {
    pub target_slug: String,
    pub target_version: String,
    pub base_slug: String,
    pub base_version: String,
    pub body: String,
    pub epoch: u32,
    pub sim: String,
    pub crm_version: u32,
    pub input_transform_version: u32,
    pub realization_params_version: u32,
    pub epoch_map_version: u32,
    pub probe_set: ProbeSetVersion,
    pub optimizer: String,
    pub emphasis: Emphasis,
    pub seed: u64,
    pub sources: PermalinkSources,
}

impl Permalink {
    pub fn canonical(&self) -> String {
        let mut out = String::from(PERMALINK_FORMAT);
        let mut push = |key: &str, value: &str| {
            out.push('|');
            out.push_str(key);
            out.push('=');
            out.push_str(&encode(value));
        };
        push(
            "look",
            &format!("{}@{}", self.target_slug, self.target_version),
        );
        push("base", &format!("{}@{}", self.base_slug, self.base_version));
        push(
            "crm",
            &format!(
                "{}@{}@{}@{}",
                self.body, self.epoch, self.sim, self.crm_version
            ),
        );
        push("input", &self.input_transform_version.to_string());
        push("realization", &self.realization_params_version.to_string());
        push("epoch_map", &self.epoch_map_version.to_string());
        push("probes", probe_set_name(self.probe_set));
        push("optimizer", &self.optimizer);
        push("emphasis", self.emphasis.as_str());
        push("seed", &self.seed.to_string());
        if let Some(value) = &self.sources.target_look {
            push("target_file", value);
        }
        if let Some(value) = &self.sources.base_look {
            push("base_file", value);
        }
        if let Some(value) = &self.sources.body {
            push("body_file", value);
        }
        if let Some(value) = &self.sources.crm {
            push("crm_file", value);
        }
        if let Some(value) = &self.sources.transform {
            push("transform_file", value);
        }
        if let Some(value) = &self.sources.covariance {
            push("covariance_file", value);
        }
        out
    }

    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let mut parts = text.split('|');
        match parts.next() {
            Some(PERMALINK_FORMAT) => {}
            _ => {
                return Err(MatchError::Permalink(format!(
                    "expected {PERMALINK_FORMAT}"
                )));
            }
        }
        let mut values: BTreeMap<String, String> = BTreeMap::new();
        for part in parts {
            let (key, value) = part
                .split_once('=')
                .ok_or_else(|| MatchError::Permalink(format!("malformed field {part:?}")))?;
            if values.insert(key.to_string(), decode(value)?).is_some() {
                return Err(MatchError::Permalink(format!("duplicate field {key}")));
            }
        }
        let required = |key: &str| -> Result<&str> {
            values
                .get(key)
                .map(String::as_str)
                .ok_or_else(|| MatchError::Permalink(format!("missing field {key}")))
        };
        let (target_slug, target_version) = split_pair(required("look")?, "look")?;
        let (base_slug, base_version) = split_pair(required("base")?, "base")?;
        let crm = required("crm")?;
        let crm_parts: Vec<&str> = crm.split('@').collect();
        if crm_parts.len() != 4 {
            return Err(MatchError::Permalink("crm field".to_string()));
        }
        let epoch = crm_parts[1]
            .parse()
            .map_err(|_| MatchError::Permalink("epoch".to_string()))?;
        let crm_version = crm_parts[3]
            .parse()
            .map_err(|_| MatchError::Permalink("crm version".to_string()))?;
        let parse_u32 = |key: &str| -> Result<u32> {
            required(key)?
                .parse()
                .map_err(|_| MatchError::Permalink(key.to_string()))
        };
        let emphasis = Emphasis::parse(required("emphasis")?)
            .ok_or_else(|| MatchError::Permalink("emphasis".to_string()))?;
        let sources = PermalinkSources {
            target_look: values.get("target_file").cloned(),
            base_look: values.get("base_file").cloned(),
            body: values.get("body_file").cloned(),
            crm: values.get("crm_file").cloned(),
            transform: values.get("transform_file").cloned(),
            covariance: values.get("covariance_file").cloned(),
        };
        Ok(Self {
            target_slug: target_slug.to_string(),
            target_version: target_version.to_string(),
            base_slug: base_slug.to_string(),
            base_version: base_version.to_string(),
            body: crm_parts[0].to_string(),
            epoch,
            sim: crm_parts[2].to_string(),
            crm_version,
            input_transform_version: parse_u32("input")?,
            realization_params_version: parse_u32("realization")?,
            epoch_map_version: parse_u32("epoch_map")?,
            probe_set: parse_probe_set(required("probes")?)?,
            optimizer: required("optimizer")?.to_string(),
            emphasis,
            seed: required("seed")?
                .parse()
                .map_err(|_| MatchError::Permalink("seed".to_string()))?,
            sources,
        })
    }
}

fn split_pair<'a>(value: &'a str, field: &str) -> Result<(&'a str, &'a str)> {
    value
        .split_once('@')
        .ok_or_else(|| MatchError::Permalink(format!("{field} field")))
}

fn probe_set_name(version: ProbeSetVersion) -> &'static str {
    match version {
        ProbeSetVersion::V1 => "v1",
    }
}

fn parse_probe_set(value: &str) -> Result<ProbeSetVersion> {
    match value {
        "v1" => Ok(ProbeSetVersion::V1),
        other => Err(MatchError::Permalink(format!("probe set {other}"))),
    }
}

fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/' | b':') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

fn decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = value
                .get(index + 1..index + 3)
                .ok_or_else(|| MatchError::Permalink("truncated escape".to_string()))?;
            let byte = u8::from_str_radix(hex, 16)
                .map_err(|_| MatchError::Permalink("escape".to_string()))?;
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| MatchError::Permalink("utf-8".to_string()))
}
