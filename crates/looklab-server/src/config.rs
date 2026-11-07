// SPDX-License-Identifier: AGPL-3.0-or-later
//! Server configuration from TOML and environment variables.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use looklab_upload::{Limits, PowParams};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PowConfig {
    pub memory_kib: u32,
    pub iterations: u32,
    pub difficulty_bits_per_image: f64,
}

impl Default for PowConfig {
    fn default() -> Self {
        let params = PowParams::default();
        Self {
            memory_kib: params.memory_kib,
            iterations: params.iterations,
            difficulty_bits_per_image: params.difficulty_bits_per_image,
        }
    }
}

impl PowConfig {
    pub fn params(self) -> PowParams {
        PowParams {
            memory_kib: self.memory_kib,
            iterations: self.iterations,
            difficulty_bits_per_image: self.difficulty_bits_per_image,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SiteConfig {
    /// Built browser distribution served at `/`, with an HTML fallback.
    pub dist_dir: Option<PathBuf>,
    /// Directory `looklab-worker` publishes into, served at `/data/`.
    pub data_dir: Option<PathBuf>,
}

/// A browser origin allowed to call a backend on another origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CorsOrigin(String);

impl CorsOrigin {
    /// # Errors
    /// Rejects anything but a bare `http`/`https` origin.
    pub fn parse(value: &str) -> Result<Self, ConfigError> {
        let url = webauthn_rs::prelude::Url::parse(value).map_err(|_| ConfigError::Value {
            name: "cors_origins",
            value: value.to_owned(),
        })?;
        let origin = url.origin().ascii_serialization();
        if origin != value.trim_end_matches('/') {
            return Err(ConfigError::Value {
                name: "cors_origins",
                value: value.to_owned(),
            });
        }
        Ok(Self(origin))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for CorsOrigin {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    bind: String,
    db_path: PathBuf,
    blob_dir: PathBuf,
    rp_id: String,
    origin: String,
    server_secret_path: PathBuf,
    bodies_dir: PathBuf,
    snapshot_dir: PathBuf,
    nonce_ttl_seconds: u64,
    registration_ttl_seconds: u64,
    body_timeout_seconds: u64,
    blob_retention_days: u32,
    /// Browser origins allowed to call the API.
    cors_origins: Vec<String>,
    site: SiteConfig,
    pow: PowConfig,
    limits: LimitsFile,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LimitsFile {
    max_body_bytes: usize,
    max_manifest_bytes: usize,
    max_images: usize,
    max_blob_bytes: usize,
    max_batches: usize,
}

impl Default for LimitsFile {
    fn default() -> Self {
        let limits = Limits::default();
        Self {
            max_body_bytes: limits.max_body_bytes,
            max_manifest_bytes: limits.max_manifest_bytes,
            max_images: limits.max_images,
            max_blob_bytes: limits.max_blob_bytes,
            max_batches: limits.max_batches,
        }
    }
}

impl LimitsFile {
    fn to_limits(&self) -> Limits {
        Limits {
            max_body_bytes: self.max_body_bytes,
            max_manifest_bytes: self.max_manifest_bytes,
            max_images: self.max_images,
            max_blob_bytes: self.max_blob_bytes,
            max_batches: self.max_batches,
        }
    }
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".to_owned(),
            db_path: PathBuf::from("looklab.sqlite3"),
            blob_dir: PathBuf::from("blobs"),
            rp_id: "looklab.example".to_owned(),
            origin: "https://looklab.example".to_owned(),
            server_secret_path: PathBuf::from("server-secret"),
            bodies_dir: PathBuf::from("data/bodies"),
            snapshot_dir: PathBuf::from("data/community"),
            nonce_ttl_seconds: 0,
            registration_ttl_seconds: 0,
            body_timeout_seconds: 30,
            blob_retention_days: 30,
            cors_origins: Vec::new(),
            site: SiteConfig::default(),
            pow: PowConfig::default(),
            limits: LimitsFile::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub db_path: PathBuf,
    pub blob_dir: PathBuf,
    pub rp_id: String,
    pub origin: String,
    pub server_secret_path: PathBuf,
    pub bodies_dir: PathBuf,
    pub snapshot_dir: PathBuf,
    pub nonce_ttl: Duration,
    pub registration_ttl: Duration,
    pub body_timeout: Duration,
    pub blob_retention_days: u32,
    pub cors_origins: Vec<CorsOrigin>,
    pub site: SiteConfig,
    pub pow: PowConfig,
    pub limits: Limits,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("configuration file: {0}")]
    File(#[from] std::io::Error),
    #[error("configuration is not valid TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("{name} is not valid: {value}")]
    Value { name: &'static str, value: String },
    #[error("{name} is out of range: {value}")]
    Range { name: &'static str, value: String },
    #[error("the operating system did not provide random bytes")]
    Random,
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let file = match path {
            Some(path) => toml::from_str::<FileConfig>(&std::fs::read_to_string(path)?)?,
            None => FileConfig::default(),
        };
        let mut config = file.params()?;
        config.apply_env()?;
        config.normalize_origin()?;
        config.validate()?;
        Ok(config)
    }

    /// Stores the ASCII serialization of the configured URL origin.
    pub fn normalize_origin(&mut self) -> Result<(), ConfigError> {
        let url =
            webauthn_rs::prelude::Url::parse(&self.origin).map_err(|_| ConfigError::Value {
                name: "origin",
                value: self.origin.clone(),
            })?;
        self.origin = url.origin().ascii_serialization();
        Ok(())
    }

    fn apply_env(&mut self) -> Result<(), ConfigError> {
        if let Some(value) = env("LOOKLAB_BIND") {
            self.bind = parse_value("LOOKLAB_BIND", &value)?;
        }
        if let Some(value) = env("LOOKLAB_DB_PATH") {
            self.db_path = PathBuf::from(value);
        }
        if let Some(value) = env("LOOKLAB_BLOB_DIR") {
            self.blob_dir = PathBuf::from(value);
        }
        if let Some(value) = env("LOOKLAB_RP_ID") {
            self.rp_id = value;
        }
        if let Some(value) = env("LOOKLAB_ORIGIN") {
            self.origin = value;
        }
        if let Some(value) = env("LOOKLAB_SERVER_SECRET_PATH") {
            self.server_secret_path = PathBuf::from(value);
        }
        if let Some(value) = env("LOOKLAB_BODIES_DIR") {
            self.bodies_dir = PathBuf::from(value);
        }
        if let Some(value) = env("LOOKLAB_SNAPSHOT_DIR") {
            self.snapshot_dir = PathBuf::from(value);
        }
        if let Some(value) = env("LOOKLAB_NONCE_TTL_SECONDS") {
            self.nonce_ttl = Duration::from_secs(parse_value("LOOKLAB_NONCE_TTL_SECONDS", &value)?);
        }
        if let Some(value) = env("LOOKLAB_REGISTRATION_TTL_SECONDS") {
            self.registration_ttl =
                Duration::from_secs(parse_value("LOOKLAB_REGISTRATION_TTL_SECONDS", &value)?);
        }
        if let Some(value) = env("LOOKLAB_BODY_TIMEOUT_SECONDS") {
            self.body_timeout =
                Duration::from_secs(parse_value("LOOKLAB_BODY_TIMEOUT_SECONDS", &value)?);
        }
        if let Some(value) = env("LOOKLAB_BLOB_RETENTION_DAYS") {
            self.blob_retention_days = parse_value("LOOKLAB_BLOB_RETENTION_DAYS", &value)?;
        }
        if let Some(value) = env("LOOKLAB_SITE_DIST") {
            self.site.dist_dir = Some(PathBuf::from(value));
        }
        if let Some(value) = env("LOOKLAB_SITE_DATA") {
            self.site.data_dir = Some(PathBuf::from(value));
        }
        if let Some(value) = env("LOOKLAB_CORS_ORIGINS") {
            self.cors_origins = parse_cors_origins(&value)?;
        }
        if let Some(value) = env("LOOKLAB_POW_MEMORY_KIB") {
            self.pow.memory_kib = parse_value("LOOKLAB_POW_MEMORY_KIB", &value)?;
        }
        if let Some(value) = env("LOOKLAB_POW_ITERATIONS") {
            self.pow.iterations = parse_value("LOOKLAB_POW_ITERATIONS", &value)?;
        }
        if let Some(value) = env("LOOKLAB_POW_DIFFICULTY_BITS") {
            self.pow.difficulty_bits_per_image =
                parse_value("LOOKLAB_POW_DIFFICULTY_BITS", &value)?;
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let url =
            webauthn_rs::prelude::Url::parse(&self.origin).map_err(|_| ConfigError::Value {
                name: "origin",
                value: self.origin.clone(),
            })?;
        if self.rp_id.is_empty() {
            return Err(ConfigError::Value {
                name: "rp_id",
                value: self.rp_id.clone(),
            });
        }
        let host = url.host_str().unwrap_or_default();
        if host != self.rp_id && !host.ends_with(&format!(".{}", self.rp_id)) {
            return Err(ConfigError::Value {
                name: "rp_id",
                value: self.rp_id.clone(),
            });
        }
        if url.scheme() != "https" && host != "localhost" && host != "127.0.0.1" {
            return Err(ConfigError::Value {
                name: "origin",
                value: self.origin.clone(),
            });
        }
        if self.nonce_ttl < Duration::from_secs(60) {
            return Err(ConfigError::Range {
                name: "nonce_ttl_seconds",
                value: self.nonce_ttl.as_secs().to_string(),
            });
        }
        if self.registration_ttl < Duration::from_secs(60) {
            return Err(ConfigError::Range {
                name: "registration_ttl_seconds",
                value: self.registration_ttl.as_secs().to_string(),
            });
        }
        if self.limits.max_body_bytes == 0 {
            return Err(ConfigError::Range {
                name: "limits.max_body_bytes",
                value: "0".to_owned(),
            });
        }
        if self.body_timeout < Duration::from_secs(1) {
            return Err(ConfigError::Range {
                name: "body_timeout_seconds",
                value: self.body_timeout.as_secs().to_string(),
            });
        }
        if self.pow.memory_kib < 8 {
            return Err(ConfigError::Range {
                name: "pow.memory_kib",
                value: self.pow.memory_kib.to_string(),
            });
        }
        if self.pow.iterations == 0 {
            return Err(ConfigError::Range {
                name: "pow.iterations",
                value: "0".to_owned(),
            });
        }
        if !self.pow.difficulty_bits_per_image.is_finite()
            || self.pow.difficulty_bits_per_image < 0.0
        {
            return Err(ConfigError::Range {
                name: "pow.difficulty_bits_per_image",
                value: self.pow.difficulty_bits_per_image.to_string(),
            });
        }
        Ok(())
    }
}

impl FileConfig {
    fn params(self) -> Result<Config, ConfigError> {
        let limits = self.limits.to_limits();
        let derived_ttl = 300 + 10 * u64::try_from(limits.max_images).unwrap_or(u64::MAX / 10);
        let nonce_ttl = if self.nonce_ttl_seconds == 0 {
            derived_ttl
        } else {
            self.nonce_ttl_seconds
        };
        let registration_ttl = if self.registration_ttl_seconds == 0 {
            derived_ttl
        } else {
            self.registration_ttl_seconds
        };
        Ok(Config {
            bind: parse_value("bind", &self.bind)?,
            db_path: self.db_path,
            blob_dir: self.blob_dir,
            rp_id: self.rp_id,
            origin: self.origin,
            server_secret_path: self.server_secret_path,
            bodies_dir: self.bodies_dir,
            snapshot_dir: self.snapshot_dir,
            nonce_ttl: Duration::from_secs(nonce_ttl),
            registration_ttl: Duration::from_secs(registration_ttl),
            body_timeout: Duration::from_secs(self.body_timeout_seconds),
            blob_retention_days: self.blob_retention_days,
            cors_origins: parse_cors_origins_list(&self.cors_origins)?,
            site: self.site,
            pow: self.pow,
            limits,
        })
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn parse_value<T>(name: &'static str, value: &str) -> Result<T, ConfigError>
where
    T: FromStr,
{
    value.parse().map_err(|_| ConfigError::Value {
        name,
        value: value.to_owned(),
    })
}

fn parse_cors_origins(value: &str) -> Result<Vec<CorsOrigin>, ConfigError> {
    value
        .split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .map(CorsOrigin::parse)
        .collect()
}

fn parse_cors_origins_list(values: &[String]) -> Result<Vec<CorsOrigin>, ConfigError> {
    values
        .iter()
        .map(|value| CorsOrigin::parse(value.trim()))
        .collect()
}

/// The serial HMAC key: 32 raw bytes or 64 hex characters in a file.
pub struct ServerSecret([u8; 32]);

impl ServerSecret {
    /// A created secret is 32 bytes of OS randomness.
    pub fn load_or_create(path: &Path) -> Result<Self, ConfigError> {
        match Self::load(path) {
            Ok(secret) => Ok(secret),
            Err(ConfigError::File(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                match Self::create(path) {
                    Ok(secret) => Ok(secret),
                    Err(ConfigError::File(error))
                        if error.kind() == std::io::ErrorKind::AlreadyExists =>
                    {
                        Self::load(path)
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    fn create(path: &Path) -> Result<Self, ConfigError> {
        use std::io::Write as _;

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| ConfigError::Random)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        tracing::info!(path = %path.display(), "generated server secret");
        Ok(Self(bytes))
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read(path)?;
        let bytes = if raw.len() == 32 {
            raw.try_into().expect("length checked")
        } else {
            let text = std::str::from_utf8(&raw)
                .map_err(|_| ConfigError::Value {
                    name: "server secret",
                    value: "not 32 bytes or hex".to_owned(),
                })?
                .trim();
            let mut out = [0u8; 32];
            if text.len() != 64 {
                return Err(ConfigError::Value {
                    name: "server secret",
                    value: "not 32 bytes or 64 hex characters".to_owned(),
                });
            }
            for (index, slot) in out.iter_mut().enumerate() {
                let high = hex_nibble(text.as_bytes()[index * 2]).ok_or(ConfigError::Value {
                    name: "server secret",
                    value: "not hex".to_owned(),
                })?;
                let low = hex_nibble(text.as_bytes()[index * 2 + 1]).ok_or(ConfigError::Value {
                    name: "server secret",
                    value: "not hex".to_owned(),
                })?;
                *slot = (high << 4) | low;
            }
            out
        };
        Ok(Self(bytes))
    }

    pub fn hmac_key(&self) -> looklab_upload::SerialHmacKey {
        looklab_upload::SerialHmacKey::new(self.0)
    }
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

impl core::fmt::Debug for ServerSecret {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ServerSecret(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cors_origins_are_bare_http_origins() {
        let origin = CorsOrigin::parse("https://looklab.example").expect("origin");
        assert_eq!(origin.as_str(), "https://looklab.example");
        assert!(CorsOrigin::parse("*").is_err());
        assert!(CorsOrigin::parse("https://looklab.example/path").is_err());
        assert!(CorsOrigin::parse("looklab.example").is_err());
    }

    #[test]
    fn secret_reads_hex_and_raw() {
        let dir = tempfile::tempdir().expect("temp dir");
        let hex_path = dir.path().join("hex");
        std::fs::write(&hex_path, format!("{}\n", "ab".repeat(32))).expect("write secret");
        let secret = ServerSecret::load(&hex_path).expect("hex secret");
        assert_eq!(secret.0, [0xab; 32]);

        let raw_path = dir.path().join("raw");
        std::fs::write(&raw_path, [7u8; 32]).expect("write secret");
        let secret = ServerSecret::load(&raw_path).expect("raw secret");
        assert_eq!(secret.0, [7u8; 32]);
    }
}
