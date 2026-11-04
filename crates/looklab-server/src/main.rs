// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::PathBuf;

const USAGE: &str = "\
looklab-server: upload intake and moderation API

usage:
  looklab-server [--config <path>]

  --config <path>  TOML configuration file
  --help           print this help

environment overrides:
  LOOKLAB_BIND, LOOKLAB_DB_PATH, LOOKLAB_BLOB_DIR, LOOKLAB_RP_ID, LOOKLAB_ORIGIN,
  LOOKLAB_SERVER_SECRET_PATH, LOOKLAB_BODIES_DIR, LOOKLAB_NONCE_TTL_SECONDS,
  LOOKLAB_REGISTRATION_TTL_SECONDS, LOOKLAB_BLOB_RETENTION_DAYS,
  LOOKLAB_POW_MEMORY_KIB, LOOKLAB_POW_ITERATIONS, LOOKLAB_POW_DIFFICULTY_BITS
";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mut config = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{USAGE}");
                return Ok(());
            }
            "--config" => {
                let Some(path) = args.next() else {
                    anyhow::bail!("--config needs a path");
                };
                config = Some(PathBuf::from(path));
            }
            other if other.starts_with("--config=") => {
                config = Some(PathBuf::from(&other["--config=".len()..]));
            }
            other => anyhow::bail!("unexpected argument {other:?}; try --help"),
        }
    }
    looklab_server::run(config).await
}
