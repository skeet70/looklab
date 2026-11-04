// SPDX-License-Identifier: AGPL-3.0-or-later
//! Offline admin path for bootstrap grants ([docs/05 §3]).

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use looklab_server::CredentialId;
use looklab_trust::{Actor, Grant, Scope, ScopeRule};
use looklab_upload::{BodyId, LookSlug};

#[derive(Debug, Args)]
pub struct AdminArgs {
    #[command(subcommand)]
    command: AdminCommand,
}

#[derive(Debug, Subcommand)]
enum AdminCommand {
    /// Grant a tier or role to a credential.
    Grant(GrantArgs),
    /// Revoke an active grant.
    Revoke(RevokeArgs),
    /// List every grant, including revoked ones.
    List(ListArgs),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum GrantArg {
    Seeder,
    Moderator,
    Admin,
}

impl From<GrantArg> for Grant {
    fn from(value: GrantArg) -> Self {
        match value {
            GrantArg::Seeder => Grant::Seeder,
            GrantArg::Moderator => Grant::Moderator,
            GrantArg::Admin => Grant::Admin,
        }
    }
}

#[derive(Debug, Args)]
struct GrantArgs {
    #[arg(long)]
    db: PathBuf,
    #[arg(long)]
    subject: String,
    #[arg(long, value_enum)]
    role: GrantArg,
    /// A rule `(body, *)`, repeatable.
    #[arg(long = "scope-body")]
    scope_body: Vec<String>,
    /// A rule `(*, look)`, repeatable.
    #[arg(long = "scope-look")]
    scope_look: Vec<String>,
    /// A rule `(body, look)` as `<body>=<look>`, repeatable.
    #[arg(long = "scope-pair")]
    scope_pair: Vec<String>,
    #[arg(long)]
    reason: String,
}

#[derive(Debug, Args)]
struct RevokeArgs {
    #[arg(long)]
    db: PathBuf,
    #[arg(long)]
    subject: String,
    #[arg(long, value_enum)]
    role: GrantArg,
    #[arg(long)]
    reason: String,
}

#[derive(Debug, Args)]
struct ListArgs {
    #[arg(long)]
    db: PathBuf,
    /// Print the records as JSON.
    #[arg(long)]
    json: bool,
}

pub fn run(args: &AdminArgs) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the async runtime")?;
    runtime.block_on(run_async(args))
}

async fn run_async(args: &AdminArgs) -> Result<()> {
    match &args.command {
        AdminCommand::Grant(args) => grant(args).await,
        AdminCommand::Revoke(args) => revoke(args).await,
        AdminCommand::List(args) => list(args).await,
    }
}

async fn open(path: &std::path::Path) -> Result<sqlx::SqlitePool> {
    looklab_server::open_database(path)
        .await
        .with_context(|| format!("opening {}", path.display()))
}

fn subject(text: &str) -> Result<CredentialId> {
    CredentialId::try_from(text).with_context(|| format!("{text} is not an internal credential id"))
}

fn scope(bodies: &[String], looks: &[String], pairs: &[String]) -> Result<Option<Scope>> {
    let mut rules = Vec::new();
    for body in bodies {
        rules.push(ScopeRule::body_all_looks(
            BodyId::new(body).with_context(|| format!("{body} is not a body id"))?,
        ));
    }
    for look in looks {
        rules.push(ScopeRule::any_body(
            LookSlug::try_from(look.as_str())
                .with_context(|| format!("{look} is not a look slug"))?,
        ));
    }
    for pair in pairs {
        let (body, look) = pair
            .split_once('=')
            .with_context(|| format!("{pair} is not <body>=<look>"))?;
        rules.push(ScopeRule::new(
            Some(BodyId::new(body).with_context(|| format!("{body} is not a body id"))?),
            Some(LookSlug::try_from(look).with_context(|| format!("{look} is not a look slug"))?),
        ));
    }
    if rules.is_empty() {
        return Ok(None);
    }
    Ok(Some(Scope::new(rules)))
}

async fn grant(args: &GrantArgs) -> Result<()> {
    let db = open(&args.db).await?;
    let scope = scope(&args.scope_body, &args.scope_look, &args.scope_pair)?;
    let record = looklab_server::admin::grant_credential(
        &db,
        Actor::Cli,
        &subject(&args.subject)?,
        args.role.into(),
        scope,
        &args.reason,
    )
    .await?;
    println!(
        "granted {} to {} ({})",
        record.grant_name(),
        record.subject,
        record.id
    );
    print_scope(record.scope_text.as_deref());
    Ok(())
}

async fn revoke(args: &RevokeArgs) -> Result<()> {
    let db = open(&args.db).await?;
    let record = looklab_server::admin::revoke_grant(
        &db,
        Actor::Cli,
        &subject(&args.subject)?,
        args.role.into(),
        &args.reason,
    )
    .await?;
    println!("revoked {} ({})", record.grant_name(), record.id);
    Ok(())
}

fn print_scope(scope: Option<&str>) {
    match scope {
        Some(scope) => println!("scope: {scope}"),
        None => println!("scope: unrestricted"),
    }
}

async fn list(args: &ListArgs) -> Result<()> {
    let db = open(&args.db).await?;
    let mut after: Option<looklab_server::GrantId> = None;
    let mut records = Vec::new();
    loop {
        let page = looklab_server::admin::list_grants(&db, after.as_ref(), 100).await?;
        records.extend(page.grants);
        match page.next_after {
            Some(id) => after = Some(id),
            None => break,
        }
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&records)?);
        return Ok(());
    }
    for record in records {
        let revoked = record
            .revoked_at
            .map(|ts| ts.to_string())
            .unwrap_or_else(|| "-".to_owned());
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            record.id,
            record.subject,
            record.grant_name(),
            record.scope_text.as_deref().unwrap_or("unrestricted"),
            record.granted_at,
            revoked
        );
    }
    Ok(())
}
