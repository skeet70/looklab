// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::PathBuf;

use looklab_server::client_errors::BuildId;
use looklab_server::operator::AdminSubject;
use looklab_server::{Config, operator};

const USAGE: &str = "\
looklab-server: upload intake, moderation API and published-site serving

usage:
  looklab-server [serve] [--config <path>]
  looklab-server init [--config <path>]
  looklab-server bootstrap-admin [--config <path>]
      (--credential <uuid> | --display-handle <name>) [--reason <text>]
  looklab-server client-errors [--config <path>] [--limit <n>] [--json]
      [--release <build-id>]

  init             create state directories, the server secret and the migrated database
  bootstrap-admin  grant admin to one registered credential through the audited CLI actor
  client-errors    print private diagnostic reports newest first (read-only; limit <= 1000)
  serve            run the HTTP server (default)

environment overrides:
  LOOKLAB_BIND, LOOKLAB_DB_PATH, LOOKLAB_BLOB_DIR, LOOKLAB_RP_ID, LOOKLAB_ORIGIN,
  LOOKLAB_SERVER_SECRET_PATH, LOOKLAB_BODIES_DIR, LOOKLAB_NONCE_TTL_SECONDS,
  LOOKLAB_REGISTRATION_TTL_SECONDS, LOOKLAB_BODY_TIMEOUT_SECONDS, LOOKLAB_BLOB_RETENTION_DAYS,
  LOOKLAB_POW_MEMORY_KIB, LOOKLAB_POW_ITERATIONS, LOOKLAB_POW_DIFFICULTY_BITS,
  LOOKLAB_SITE_DIST, LOOKLAB_SITE_DATA, LOOKLAB_CORS_ORIGINS
";

#[derive(Debug)]
enum Command {
    Serve(Option<PathBuf>),
    Init(Option<PathBuf>),
    BootstrapAdmin {
        config: Option<PathBuf>,
        credential: Option<String>,
        display_handle: Option<String>,
        reason: String,
    },
    ClientErrors {
        config: Option<PathBuf>,
        limit: usize,
        json: bool,
        release: Option<BuildId>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let command = parse()?;
    match command {
        Command::Serve(config) => looklab_server::run(config).await,
        Command::Init(config) => {
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::INFO)
                .with_ansi(false)
                .init();
            let config = Config::load(config.as_deref())?;
            operator::init(&config).await?;
            println!("initialised {}", config.db_path.display());
            Ok(())
        }
        Command::BootstrapAdmin {
            config,
            credential,
            display_handle,
            reason,
        } => {
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::INFO)
                .with_ansi(false)
                .init();
            let config = Config::load(config.as_deref())?;
            let subject = match (credential, display_handle) {
                (Some(credential), None) => AdminSubject::credential(&credential)?,
                (None, Some(handle)) => AdminSubject::display_handle(&handle)?,
                _ => anyhow::bail!("pass exactly one of --credential or --display-handle"),
            };
            let record = operator::bootstrap_admin(&config, subject, &reason).await?;
            println!(
                "granted {} to {} ({})",
                record.grant_name(),
                record.subject,
                record.id
            );
            Ok(())
        }
        Command::ClientErrors {
            config,
            limit,
            json,
            release,
        } => {
            let config = Config::load(config.as_deref())?;
            let reports = operator::client_error_reports(&config, limit, release.as_ref()).await?;
            operator::write_client_error_reports(std::io::stdout().lock(), &reports, json)?;
            Ok(())
        }
    }
}

fn parse() -> anyhow::Result<Command> {
    let mut args = std::env::args().skip(1).peekable();
    let command = match args.peek().map(String::as_str) {
        Some("init") => {
            args.next();
            Command::Init(None)
        }
        Some("bootstrap-admin") => {
            args.next();
            Command::BootstrapAdmin {
                config: None,
                credential: None,
                display_handle: None,
                reason: "first admin bootstrap".to_owned(),
            }
        }
        Some("client-errors") => {
            args.next();
            Command::ClientErrors {
                config: None,
                limit: 50,
                json: false,
                release: None,
            }
        }
        Some("serve") => {
            args.next();
            Command::Serve(None)
        }
        _ => Command::Serve(None),
    };
    let mut command = command;
    while let Some(arg) = args.next() {
        match &mut command {
            Command::Serve(config) | Command::Init(config) => match arg.as_str() {
                "--help" | "-h" => {
                    print!("{USAGE}");
                    std::process::exit(0);
                }
                "--config" => {
                    let Some(path) = args.next() else {
                        anyhow::bail!("--config needs a path");
                    };
                    *config = Some(PathBuf::from(path));
                }
                other if other.starts_with("--config=") => {
                    *config = Some(PathBuf::from(&other["--config=".len()..]));
                }
                other => anyhow::bail!("unexpected argument {other:?}; try --help"),
            },
            Command::BootstrapAdmin {
                config,
                credential,
                display_handle,
                reason,
            } => match arg.as_str() {
                "--help" | "-h" => {
                    print!("{USAGE}");
                    std::process::exit(0);
                }
                "--config" => {
                    let Some(path) = args.next() else {
                        anyhow::bail!("--config needs a path");
                    };
                    *config = Some(PathBuf::from(path));
                }
                "--credential" => {
                    let Some(value) = args.next() else {
                        anyhow::bail!("--credential needs an internal credential id");
                    };
                    *credential = Some(value);
                }
                "--display-handle" => {
                    let Some(value) = args.next() else {
                        anyhow::bail!("--display-handle needs a name");
                    };
                    *display_handle = Some(value);
                }
                "--reason" => {
                    let Some(value) = args.next() else {
                        anyhow::bail!("--reason needs text");
                    };
                    *reason = value;
                }
                other if other.starts_with("--config=") => {
                    *config = Some(PathBuf::from(&other["--config=".len()..]));
                }
                other => anyhow::bail!("unexpected argument {other:?}; try --help"),
            },
            Command::ClientErrors {
                config,
                limit,
                json,
                release,
            } => match arg.as_str() {
                "--help" | "-h" => {
                    print!("{USAGE}");
                    std::process::exit(0);
                }
                "--config" => {
                    let Some(path) = args.next() else {
                        anyhow::bail!("--config needs a path");
                    };
                    *config = Some(PathBuf::from(path));
                }
                "--limit" => {
                    let Some(value) = args.next() else {
                        anyhow::bail!("--limit needs a number");
                    };
                    *limit = parse_limit(&value)?;
                }
                "--json" => *json = true,
                "--release" => {
                    let Some(value) = args.next() else {
                        anyhow::bail!("--release needs a build id");
                    };
                    *release = Some(BuildId::parse(&value).map_err(|_| {
                        anyhow::anyhow!("--release needs 64 lowercase hexadecimal characters")
                    })?);
                }
                other if other.starts_with("--config=") => {
                    *config = Some(PathBuf::from(&other["--config=".len()..]));
                }
                other if other.starts_with("--limit=") => {
                    *limit = parse_limit(&other["--limit=".len()..])?;
                }
                other if other.starts_with("--release=") => {
                    let value = &other["--release=".len()..];
                    *release = Some(BuildId::parse(value).map_err(|_| {
                        anyhow::anyhow!("--release needs 64 lowercase hexadecimal characters")
                    })?);
                }
                other => anyhow::bail!("unexpected argument {other:?}; try --help"),
            },
        }
    }
    Ok(command)
}

fn parse_limit(value: &str) -> anyhow::Result<usize> {
    let limit: usize = value
        .parse()
        .map_err(|_| anyhow::anyhow!("--limit needs a number"))?;
    if !(1..=1_000).contains(&limit) {
        anyhow::bail!("--limit must be between 1 and 1000");
    }
    Ok(limit)
}
