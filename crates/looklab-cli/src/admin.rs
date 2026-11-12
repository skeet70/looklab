// SPDX-License-Identifier: AGPL-3.0-or-later
//! Offline admin path for grants, Look anchors and LUT admissions.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use looklab_model::file::{NEUTRAL_BASELINE_SLUG, NEUTRAL_BASELINE_VERSION};
use looklab_schema::{
    LicenseDeclaration, LookId, LutBase, LutDynamicRange, LutEmulationDecl, LutEncoding, LutFile,
    LutLookProposal, LutLookVersion, LutRenderer, Sha256Hex, SimKey, look_id_from_name,
};
use looklab_server::{FsBlobStore, LutAdmission, admit_lut};
use looklab_trust::{Actor, Grant, Scope, ScopeRule};
use looklab_upload::{BodyId, LookSlug};
use serde::Deserialize;
use sha2::{Digest, Sha256};

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
    /// Start a Look's cumulative anchor at a released version.
    ReAnchor(ReAnchorArgs),
    /// Admit a community LUT as an approved custom look.
    ImportLut(Box<ImportLutArgs>),
    /// Revert a Look to a published version and reject named contributions.
    Revert(RevertArgs),
    /// Block a profile from every future intake route.
    BlockIntake(BlockIntakeArgs),
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

#[derive(Debug, Args)]
struct ReAnchorArgs {
    #[arg(long)]
    db: PathBuf,
    #[arg(long)]
    look: String,
    #[arg(long)]
    version: u32,
    #[arg(long)]
    reason: String,
}

#[derive(Debug, Args)]
struct RevertArgs {
    #[arg(long)]
    db: PathBuf,
    #[arg(long)]
    blobs: PathBuf,
    #[arg(long)]
    look: String,
    /// The published version to restore.
    #[arg(long)]
    version: u32,
    /// A contribution to reject; repeatable.
    #[arg(long = "reject-contribution")]
    reject_contribution: Vec<String>,
    #[arg(long)]
    reason: String,
}

#[derive(Debug, Args)]
struct BlockIntakeArgs {
    #[arg(long)]
    db: PathBuf,
    /// The internal credential id of the profile to block.
    #[arg(long)]
    profile: String,
    #[arg(long)]
    reason: String,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EncodingArg {
    /// Scene-referred.
    Flog2Fgamut,
    /// Display-referred, expecting the fixed neutral baseline.
    Neutral,
    /// Display-referred, expecting a published Look version.
    Look,
    /// Display-referred, expecting a body rendering.
    Body,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RendererArg {
    XRawStudio,
    /// The donor named no rendering engine.
    Unspecified,
}

impl From<RendererArg> for LutRenderer {
    fn from(value: RendererArg) -> Self {
        match value {
            RendererArg::XRawStudio => Self::XRawStudio,
            RendererArg::Unspecified => Self::Unspecified,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DynamicRangeArg {
    Dr100,
    Dr200,
    Dr400,
}

impl From<DynamicRangeArg> for LutDynamicRange {
    fn from(value: DynamicRangeArg) -> Self {
        match value {
            DynamicRangeArg::Dr100 => Self::Dr100,
            DynamicRangeArg::Dr200 => Self::Dr200,
            DynamicRangeArg::Dr400 => Self::Dr400,
        }
    }
}

#[derive(Debug, Args)]
struct ImportLutArgs {
    #[arg(long)]
    db: PathBuf,
    #[arg(long)]
    blobs: PathBuf,
    /// Repository manifest to read one entry from.
    #[arg(long, requires = "id")]
    manifest: Option<PathBuf>,
    /// The manifest entry's `look` slug.
    #[arg(long, requires = "manifest")]
    id: Option<String>,
    /// The `.cube` file; defaults to the manifest entry's `file`.
    #[arg(long)]
    cube: Option<PathBuf>,
    /// The Look's name; the slug is generated from it.
    #[arg(long)]
    name: Option<String>,
    /// The redistribution license the donor declared.
    #[arg(long, conflicts_with = "own_work")]
    license: Option<String>,
    /// The donor's own work, with no separate source license.
    #[arg(long)]
    own_work: bool,
    /// Credit for the donor.
    #[arg(long)]
    attribution: Option<String>,
    /// Where the donor published the LUT.
    #[arg(long)]
    source_url: Option<String>,
    #[arg(long, value_enum)]
    encoding: Option<EncodingArg>,
    /// The published base Look slug for `--encoding look`.
    #[arg(long)]
    base_look: Option<String>,
    /// The published base Look version for `--encoding look`.
    #[arg(long)]
    base_version: Option<String>,
    /// The body id for `--encoding body`.
    #[arg(long)]
    base_body: Option<String>,
    /// The rendering engine for `--encoding body`.
    #[arg(long, value_enum)]
    base_renderer: Option<RendererArg>,
    /// The film simulation for `--encoding body`.
    #[arg(long)]
    base_sim: Option<String>,
    /// The dynamic range for `--encoding body`.
    #[arg(long, value_enum)]
    base_dr: Option<DynamicRangeArg>,
    /// Notes stored with the declared body rendering.
    #[arg(long)]
    base_notes: Option<String>,
    /// The camera maker the LUT emulates.
    #[arg(long)]
    emulates_camera: Option<String>,
    /// The camera rendering the LUT emulates.
    #[arg(long)]
    emulates_rendering: Option<String>,
    /// The film stock the LUT emulates.
    #[arg(long)]
    emulates_film_stock: Option<String>,
    /// The operator's admission reason; overrides a manifest entry's reason.
    #[arg(long)]
    reason: Option<String>,
    /// Admit a manifest entry whose `role` is not `original`.
    #[arg(long, requires = "manifest")]
    allow_non_original: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LutManifest {
    donor: Donor,
    #[serde(default, rename = "donor_body")]
    _donor_body: Option<DonorBody>,
    #[serde(default, rename = "lut")]
    luts: Vec<ManifestLut>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Donor {
    #[serde(rename = "handle")]
    _handle: String,
    #[serde(rename = "source")]
    _source: String,
    #[serde(rename = "date")]
    _date: String,
    license: String,
    attribution: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DonorBody {
    #[serde(rename = "body")]
    _body: String,
    #[serde(rename = "notes")]
    _notes: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestLut {
    name: String,
    look: String,
    file: String,
    #[serde(rename = "title")]
    _title: String,
    sha256: String,
    #[serde(default, rename = "role")]
    _role: Option<String>,
    #[serde(rename = "provenance")]
    _provenance: String,
    #[serde(default, rename = "donor_body")]
    _donor_body: Option<String>,
    #[serde(default)]
    emulates: Option<LutEmulationDecl>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    attribution: Option<String>,
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    encoding: ManifestEncoding,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ManifestEncoding {
    Flog2Fgamut,
    Display { base: LutBase },
}

impl From<ManifestEncoding> for LutEncoding {
    fn from(value: ManifestEncoding) -> Self {
        match value {
            ManifestEncoding::Flog2Fgamut => Self::Flog2Fgamut,
            ManifestEncoding::Display { base } => Self::Display { base },
        }
    }
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
        AdminCommand::ReAnchor(args) => reanchor(args).await,
        AdminCommand::ImportLut(args) => import_lut(args).await,
        AdminCommand::Revert(args) => revert(args).await,
        AdminCommand::BlockIntake(args) => block_intake(args).await,
    }
}

async fn open(path: &Path) -> Result<sqlx::SqlitePool> {
    looklab_server::open_database(path)
        .await
        .with_context(|| format!("opening {}", path.display()))
}

fn subject(text: &str) -> Result<looklab_server::CredentialId> {
    looklab_server::CredentialId::try_from(text)
        .with_context(|| format!("{text} is not an internal credential id"))
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

async fn reanchor(args: &ReAnchorArgs) -> Result<()> {
    let db = open(&args.db).await?;
    let look = LookSlug::try_from(args.look.as_str())
        .with_context(|| format!("{} is not a look slug", args.look))?;
    let record =
        looklab_server::admin::reanchor(&db, Actor::Cli, &look, args.version, &args.reason).await?;
    println!(
        "anchored {} at version {} ({})",
        record.look, record.version, record.reason
    );
    Ok(())
}

async fn import_lut(args: &ImportLutArgs) -> Result<()> {
    if !args.db.is_file() {
        bail!("{} is not an existing database file", args.db.display());
    }
    if !args.blobs.is_dir() {
        bail!("{} is not an existing blob directory", args.blobs.display());
    }
    let (bytes, proposal, reason) = admission(args)?;
    let db = open(&args.db).await?;
    let blobs = FsBlobStore::new(&args.blobs)
        .with_context(|| format!("opening blob storage at {}", args.blobs.display()))?;
    let admitted = admit_lut(
        &db,
        &blobs,
        LutAdmission {
            proposal,
            cube: bytes,
            reason,
        },
    )
    .await
    .context("admitting the LUT")?;
    println!(
        "admitted {} as proposal {} (approved by cli); the import job is queued",
        admitted.look, admitted.proposal
    );
    Ok(())
}

async fn revert(args: &RevertArgs) -> Result<()> {
    if !args.db.is_file() {
        bail!("{} is not an existing database file", args.db.display());
    }
    if !args.blobs.is_dir() {
        bail!("{} is not an existing blob directory", args.blobs.display());
    }
    let look = LookSlug::try_from(args.look.as_str())
        .with_context(|| format!("{} is not a look slug", args.look))?;
    let rejected = args
        .reject_contribution
        .iter()
        .map(|id| {
            looklab_server::ContributionId::try_from(id.as_str())
                .with_context(|| format!("{id} is not an internal contribution id"))
        })
        .collect::<Result<Vec<_>>>()?;
    let db = open(&args.db).await?;
    let record = looklab_server::admin::revert(
        &db,
        &args.blobs,
        Actor::Cli,
        &look,
        args.version,
        &rejected,
        &args.reason,
    )
    .await
    .context("reverting the Look")?;
    let restoration = match record.restoration {
        looklab_server::admin::Restoration::Current => "current".to_owned(),
        looklab_server::admin::Restoration::HistoricalNeedsRebuild { reason } => {
            format!("historical copy that needs rebuilding ({reason:?})")
        }
    };
    println!(
        "reverted {} to version {} as version {} (approved by cli)",
        record.look, record.target_version, record.new_version
    );
    println!("restoration: {restoration}");
    println!("rejected contributions: {}", record.rejected.len());
    println!("dependent LUT rebases: {}", record.lut_dependents.len());
    println!("refit queued: {}", record.refit_queued);
    Ok(())
}

async fn block_intake(args: &BlockIntakeArgs) -> Result<()> {
    if !args.db.is_file() {
        bail!("{} is not an existing database file", args.db.display());
    }
    let db = open(&args.db).await?;
    let profile = subject(&args.profile)?;
    let record =
        looklab_server::admin::block_intake(&db, Actor::Cli, &profile, &args.reason).await?;
    println!(
        "blocked intake for {} at {} (approved by cli)",
        record.credential, record.blocked_at
    );
    Ok(())
}

fn admission(args: &ImportLutArgs) -> Result<(Vec<u8>, LutLookProposal, String)> {
    match &args.manifest {
        Some(path) => manifest_admission(args, path),
        None => explicit_admission(args),
    }
}

fn manifest_admission(
    args: &ImportLutArgs,
    path: &Path,
) -> Result<(Vec<u8>, LutLookProposal, String)> {
    reject_explicit(args)?;
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let manifest: LutManifest = toml::from_str(&text)
        .with_context(|| format!("{} is not a LUT manifest", path.display()))?;
    let id = args
        .id
        .as_deref()
        .context("--id is required with --manifest")?;
    let entry = manifest
        .luts
        .iter()
        .find(|entry| entry.look == id)
        .with_context(|| format!("{id} is not an entry in {}", path.display()))?;
    let role = entry._role.as_deref().unwrap_or("original");
    if role != "original" && !args.allow_non_original {
        bail!("{id} has role {role:?}; pass --allow-non-original to admit it");
    }
    let cube = args.cube.clone().unwrap_or_else(|| {
        path.parent()
            .unwrap_or_else(|| Path::new("."))
            .join(&entry.file)
    });
    let bytes = std::fs::read(&cube).with_context(|| format!("reading {}", cube.display()))?;
    let size = u64::try_from(bytes.len()).context("cube size")?;
    let proposal = LutLookProposal {
        look: LookId::new(&entry.look)
            .with_context(|| format!("{} is not a look slug", entry.look))?,
        name: entry.name.clone(),
        license: LicenseDeclaration {
            own_work: false,
            source_license: Some(
                entry
                    .license
                    .clone()
                    .unwrap_or_else(|| manifest.donor.license.clone()),
            ),
            source_url: entry.source_url.clone(),
        },
        attribution: entry
            .attribution
            .clone()
            .unwrap_or_else(|| manifest.donor.attribution.clone()),
        lut: LutFile {
            file: entry.file.clone(),
            sha256: Sha256Hex::parse(&entry.sha256)
                .with_context(|| format!("{} is not a sha256", entry.sha256))?,
            size,
        },
        encoding: entry.encoding.clone().into(),
        emulates: entry.emulates.clone(),
    };
    let reason = args
        .reason
        .clone()
        .or_else(|| entry.reason.clone())
        .with_context(|| format!("{id} has no reason; pass --reason"))?;
    Ok((bytes, proposal, reason))
}

fn explicit_admission(args: &ImportLutArgs) -> Result<(Vec<u8>, LutLookProposal, String)> {
    let cube = args
        .cube
        .as_ref()
        .context("--cube is required without --manifest")?;
    let name = args
        .name
        .as_ref()
        .context("--name is required without --manifest")?;
    let reason = args
        .reason
        .clone()
        .context("--reason is required without --manifest")?;
    let attribution = args
        .attribution
        .clone()
        .context("--attribution is required without --manifest")?;
    let bytes = std::fs::read(cube).with_context(|| format!("reading {}", cube.display()))?;
    let size = u64::try_from(bytes.len()).context("cube size")?;
    let file = cube
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("{} has no file name", cube.display()))?
        .to_owned();
    let proposal = LutLookProposal {
        look: look_id_from_name(name)?,
        name: name.clone(),
        license: LicenseDeclaration {
            own_work: args.own_work,
            source_license: args.license.clone(),
            source_url: args.source_url.clone(),
        },
        attribution,
        lut: LutFile {
            file,
            sha256: Sha256Hex::parse(&sha256_hex(&bytes))?,
            size,
        },
        encoding: explicit_encoding(args)?,
        emulates: explicit_emulation(args)?,
    };
    Ok((bytes, proposal, reason))
}

fn explicit_encoding(args: &ImportLutArgs) -> Result<LutEncoding> {
    let encoding = args
        .encoding
        .context("--encoding is required without --manifest")?;
    let base = match encoding {
        EncodingArg::Flog2Fgamut => return Ok(LutEncoding::Flog2Fgamut),
        EncodingArg::Neutral => LutBase::Look {
            look: LookId::new(NEUTRAL_BASELINE_SLUG)?,
            version: LutLookVersion::new(NEUTRAL_BASELINE_VERSION)?,
        },
        EncodingArg::Look => LutBase::Look {
            look: LookId::new(
                args.base_look
                    .as_deref()
                    .context("--base-look is required for --encoding look")?,
            )?,
            version: LutLookVersion::new(
                args.base_version
                    .as_deref()
                    .context("--base-version is required for --encoding look")?,
            )?,
        },
        EncodingArg::Body => LutBase::BodyRealization {
            body: BodyId::new(
                args.base_body
                    .as_deref()
                    .context("--base-body is required for --encoding body")?,
            )?,
            renderer: args
                .base_renderer
                .unwrap_or(RendererArg::Unspecified)
                .into(),
            film_simulation: args.base_sim.as_deref().map(SimKey::new).transpose()?,
            dynamic_range: args.base_dr.map(Into::into),
            notes: args.base_notes.clone().unwrap_or_default(),
        },
    };
    Ok(LutEncoding::Display { base })
}

fn explicit_emulation(args: &ImportLutArgs) -> Result<Option<LutEmulationDecl>> {
    match (
        &args.emulates_camera,
        &args.emulates_rendering,
        &args.emulates_film_stock,
    ) {
        (None, None, None) => Ok(None),
        (Some(maker), Some(rendering), None) => Ok(Some(LutEmulationDecl::CameraRendering {
            maker: maker.clone(),
            rendering: rendering.clone(),
        })),
        (None, None, Some(name)) => Ok(Some(LutEmulationDecl::FilmStock { name: name.clone() })),
        _ => bail!("use --emulates-camera with --emulates-rendering, or --emulates-film-stock"),
    }
}

fn reject_explicit(args: &ImportLutArgs) -> Result<()> {
    let explicit = [
        (args.name.is_some(), "--name"),
        (args.license.is_some(), "--license"),
        (args.own_work, "--own-work"),
        (args.attribution.is_some(), "--attribution"),
        (args.source_url.is_some(), "--source-url"),
        (args.encoding.is_some(), "--encoding"),
        (args.base_look.is_some(), "--base-look"),
        (args.base_version.is_some(), "--base-version"),
        (args.base_body.is_some(), "--base-body"),
        (args.base_renderer.is_some(), "--base-renderer"),
        (args.base_sim.is_some(), "--base-sim"),
        (args.base_dr.is_some(), "--base-dr"),
        (args.base_notes.is_some(), "--base-notes"),
        (args.emulates_camera.is_some(), "--emulates-camera"),
        (args.emulates_rendering.is_some(), "--emulates-rendering"),
        (args.emulates_film_stock.is_some(), "--emulates-film-stock"),
    ];
    if let Some((_, flag)) = explicit.into_iter().find(|(set, _)| *set) {
        bail!("{flag} cannot be combined with --manifest");
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
