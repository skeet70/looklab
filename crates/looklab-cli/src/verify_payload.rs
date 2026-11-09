// SPDX-License-Identifier: AGPL-3.0-or-later
//! `looklab verify-payload`: reproduce a browser upload from a HAR under
//! wasmtime and compare it byte for byte.
//!
//! The flow is [docs/06 §5](../../../docs/06-privacy-and-payload.md) steps 1-4.
//! Nothing the site served beyond the HAR itself is consulted, and no keys.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use clap::{Args, ValueEnum};
use looklab_meta::{ParsedMetadata, RawSerial};
use looklab_prep::{LocalImageKey, PrepParams, RawFormat, SeedFile};
use looklab_prep_module::{
    BuildManifestRequest, BuildManifestResponse, Function, ManifestImageInput, ModuleError,
    PrepareRequest, PrepareResponse, PriorTransformSpec, Response, VersionResponse,
};
use looklab_schema::{
    ConfirmationScope as SchemaConfirmationScope, Finding as SchemaFinding, LookId,
    MAX_CONFIRMATION_BYTES, MakernoteValues, ManualCatalog, MappingError, MonoFilter,
    RenderClass as SchemaRenderClass, SettingSource, Sha256Hex, SimKey, applicable_controls,
    classify_card_render, classify_render, merge_manual_catalogs, settings_from_makernote_for,
    sim_from_makernote, validate_manual_catalog,
};
use looklab_server::body_schema::{Bodies, BodyRecord, resolve_epoch, validate_settings};
use looklab_server::{RequestDomain, StatusToken, request_digest};
use looklab_upload::{
    BatchManifest, BlobKind, BodyId, CardDeclaration, Challenge, CommunitySnapshotVersion,
    ContributionKind, ControlEntry, ControlKey, ControlSettings, DEFAULT_MAX_ATTESTATIONS,
    DEFAULT_MAX_BATCHES, Envelope, EnvelopeParts, ExpectedAssertion, FirmwareId, FirmwareVersion,
    FirmwareVersionString, Intent, Limits, LookSlug, ManifestHash, ManifestImage, ManifestMetadata,
    ManualWithdrawalRequest, MonochromeAuthorization, Nonce, PowParams, RawSerial as UploadSerial,
    RenderClass, SendManifest, SignatureAlgorithm, StepIndex, StructuralVerdict, TopOriginReview,
    WebAuthnAssertion, canonical_send_manifest_bytes, check_structure, encode_envelope,
    parse_envelope_strict, send_manifest_hash, verify as verify_pow,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use wasmtime::{Engine, Instance, Module, Store};

const STATE_FORMAT: &str = "looklab-verify-state/1";
const UPLOAD_PATH: &str = "/api/upload";
const MANUAL_CONFIRMATION_PATH: &str = "/api/manual-confirmations";
const MANUAL_WITHDRAWAL_PATH: &str = "/api/manual-confirmations/withdraw";
const MANUAL_SUMMARY_PREFIX: &str = "/api/manual-confirmations/";
const NONCE_PATH: &str = "/api/nonce";
const SESSION_PATH: &str = "/api/session";
const REGISTER_START_PATH: &str = "/api/passkey/register/start";
const REGISTER_FINISH_PATH: &str = "/api/passkey/register/finish";
const STATUS_PREFIX: &str = "/status/";
const FILM_MODE_TAG: &str = "FilmMode";

/// The residual channels [docs/06 §5](../../../docs/06-privacy-and-payload.md)
/// documents as not closed.
const RESIDUAL_CHANNELS: &[&str] = &[
    "Residual channels (documented, not closed):",
    "- a client can choose which valid proof-of-work solutions it sends, leaking a few bits per batch at exponentially more work;",
    "- the signature itself: one signature per send, because every batch must carry the same assertion; a site that forges keys can choose its bytes freely (tens to hundreds of bytes per send, depending on algorithm);",
    "- the credential ID chosen at passkey registration: a forged-key site picks it once (≤ 255 bytes, a one-time leak; it's then fixed by the identical-ID check);",
    "- timing.",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DeclaredKind {
    Library,
    Rerender,
    Pattern,
    Gradient,
    Chart,
}

impl From<DeclaredKind> for ContributionKind {
    fn from(value: DeclaredKind) -> Self {
        match value {
            DeclaredKind::Library => Self::Library,
            DeclaredKind::Rerender => Self::Rerender,
            DeclaredKind::Pattern => Self::Pattern,
            DeclaredKind::Gradient => Self::Gradient,
            DeclaredKind::Chart => Self::Chart,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DeclaredIntent {
    Native,
    Recipe,
    Card,
}

/// The explicit contribution authorization the upload's monochrome batches carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DeclaredAuthorization {
    #[value(name = "model-development-180-days")]
    ModelDevelopment180Days,
}

impl From<DeclaredAuthorization> for MonochromeAuthorization {
    fn from(value: DeclaredAuthorization) -> Self {
        match value {
            DeclaredAuthorization::ModelDevelopment180Days => Self::ModelDevelopment180Days,
        }
    }
}

#[derive(Debug, Args)]
pub struct VerifyPayloadArgs {
    /// The local seed file exported by the client. SENSITIVE: it reveals sampling order.
    #[arg(long, value_name = "FILE")]
    pub seeds: Option<PathBuf>,
    /// HAR files exported from the browser request log; repeat for several sessions.
    #[arg(long = "har", value_name = "FILE", required = true)]
    pub har: Vec<PathBuf>,
    /// The source-built `looklab-prep-module` wasm binary.
    #[arg(long, value_name = "FILE")]
    pub module: PathBuf,
    /// The site's origin, for example `https://looklab.example`.
    #[arg(long, value_name = "URL")]
    pub origin: String,
    /// The site's WebAuthn relying-party ID, the registrable domain.
    #[arg(long = "rp-id", value_name = "DOMAIN")]
    pub rp_id: String,
    /// The source-built `data/` snapshot used to resolve bodies and controls.
    #[arg(long, value_name = "DIR", default_value = "data")]
    pub data: PathBuf,
    /// The contribution kind you declared to the site.
    #[arg(long, value_enum, value_name = "KIND")]
    pub kind: DeclaredKind,
    /// The fit intent you declared to the site.
    #[arg(long, value_enum, value_name = "INTENT")]
    pub intent: DeclaredIntent,
    /// The recipe's target look, required with `--intent recipe`.
    #[arg(long = "target-look", value_name = "SLUG")]
    pub target_look: Option<String>,
    /// The card firmware, as `<firmware-id>/<firmware-version>`, for `--intent card`
    /// when no image carries a render tag.
    #[arg(long, value_name = "ID/VERSION")]
    pub firmware: Option<String>,
    /// A control value you entered by hand, as `key=step`. Repeat for each.
    #[arg(long = "declare-setting", value_name = "KEY=STEP")]
    pub declare_setting: Vec<String>,
    /// The contributor's explicit monochrome model-development authorization; required for monochrome batches.
    #[arg(
        long = "monochrome-authorization",
        value_enum,
        value_name = "AUTHORIZATION"
    )]
    pub monochrome_authorization: Option<DeclaredAuthorization>,
    /// Where the credential id is recorded between runs. Defaults to
    /// `$XDG_STATE_HOME/looklab/verify.json`, else `~/.local/state/looklab/verify.json`.
    #[arg(long, value_name = "FILE")]
    pub state: Option<PathBuf>,
    /// Proof-of-work memory in KiB. Defaults to the server default, 16 MiB.
    #[arg(long = "pow-memory-kib", value_name = "KIB")]
    pub pow_memory_kib: Option<u32>,
    /// Proof-of-work iterations. Defaults to the server default, 1.
    #[arg(long = "pow-iterations", value_name = "N")]
    pub pow_iterations: Option<u32>,
    /// Proof-of-work difficulty in bits per image. Defaults to the server default, 8.
    #[arg(long = "pow-difficulty", value_name = "BITS")]
    pub pow_difficulty: Option<f64>,
    /// The RAW and JPEG files that went into the upload, each RAW before its JPEGs.
    #[arg(value_name = "FILES")]
    pub files: Vec<PathBuf>,
}

pub fn run(args: &VerifyPayloadArgs) -> Result<()> {
    let findings = verify(args)?;
    print_report(&findings);
    if findings.status() == Status::Fail {
        bail!("payload verification failed");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Status {
    Pass,
    Review,
    Fail,
}

#[derive(Debug, Default)]
struct Findings {
    lines: Vec<String>,
    review: Vec<String>,
    failures: Vec<String>,
}

impl Findings {
    fn note(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }

    fn fail(&mut self, reason: impl Into<String>) {
        self.failures.push(reason.into());
    }

    fn review(&mut self, item: impl Into<String>) {
        self.review.push(item.into());
    }

    fn status(&self) -> Status {
        if !self.failures.is_empty() {
            Status::Fail
        } else if !self.review.is_empty() {
            Status::Review
        } else {
            Status::Pass
        }
    }
}

fn print_report(findings: &Findings) {
    for line in &findings.lines {
        println!("{line}");
    }
    match findings.status() {
        Status::Pass => println!("\nPASS"),
        Status::Review => {
            println!("\nREVIEW NEEDED");
            for item in &findings.review {
                println!("  - {item}");
            }
        }
        Status::Fail => {
            println!("\nFAIL");
            for reason in &findings.failures {
                println!("  - {reason}");
            }
            if !findings.review.is_empty() {
                println!("\nREVIEW NEEDED");
                for item in &findings.review {
                    println!("  - {item}");
                }
            }
        }
    }
    println!();
    for line in RESIDUAL_CHANNELS {
        println!("{line}");
    }
}

fn verify(args: &VerifyPayloadArgs) -> Result<Findings> {
    let origin = Origin::parse(&args.origin)
        .with_context(|| format!("--origin {:?} is not an origin", args.origin))?;
    if args.rp_id.trim().is_empty() {
        bail!("--rp-id must not be empty");
    }
    let intent = intent_before_files(args)?;
    let bodies = Bodies::load(&args.data.join("bodies"), &args.data.join("community"))
        .with_context(|| format!("loading bodies from {}", args.data.display()))?;
    if bodies.is_empty() {
        bail!(
            "{} has no body entities",
            args.data.join("bodies").display()
        );
    }
    let declarations = parse_declared_settings(&args.declare_setting)?;
    let hars: Vec<(PathBuf, Har)> = args
        .har
        .iter()
        .map(|path| Ok((path.clone(), Har::load(path)?)))
        .collect::<Result<_>>()?;

    let has_uploads = hars.iter().any(|(_, har)| {
        har.log
            .entries
            .iter()
            .any(|entry| classify(&entry.request, &origin) == RequestClass::Upload)
    });
    let has_withdrawals = hars.iter().any(|(_, har)| {
        har.log
            .entries
            .iter()
            .any(|entry| classify(&entry.request, &origin) == RequestClass::ManualWithdrawal)
    });
    let manuals = if has_withdrawals {
        Some(load_manual_catalog(&args.data)?)
    } else {
        None
    };

    let seeds = match args.seeds.as_deref() {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            Some(
                SeedFile::from_json(&text)
                    .with_context(|| format!("parsing {}", path.display()))?,
            )
        }
        None if has_uploads => bail!("--seeds is required when the HAR contains an upload"),
        None => None,
    };
    let module = ModuleHost::open(&args.module)?;
    let groups = if has_uploads {
        if args.files.is_empty() {
            bail!("RAW and JPEG files are required when the HAR contains an upload");
        }
        load_groups(&args.files)?
    } else {
        if !args.files.is_empty() {
            bail!("RAW and JPEG files were supplied but the HAR contains no upload to cover");
        }
        Vec::new()
    };
    let state_path = state_path(args.state.as_deref())?;
    let mut state = VerifyState::load(&state_path)?;

    let mut findings = Findings::default();
    findings.note("looklab verify-payload");
    findings.note(format!("origin: {origin}"));
    findings.note(format!("rp id: {}", args.rp_id));
    let version = module.version()?;
    findings.note(format!(
        "module: {} (prep version {}, build id {})",
        args.module.display(),
        version.prep_version,
        hex(&version.build_id)
    ));
    findings.note(match (&args.seeds, &seeds) {
        (Some(path), Some(seeds)) => format!(
            "seeds: {} (prior-transform version {})",
            path.display(),
            seeds.prior_transform_version().get()
        ),
        _ => "seeds: none".to_owned(),
    });
    findings.note(format!("state: {}", state_path.display()));
    findings.note(format!(
        "files: {} in {} group(s)",
        args.files.len(),
        groups.len()
    ));

    let serials: Vec<(&str, &RawSerial)> = groups
        .iter()
        .flat_map(Group::files)
        .filter_map(|file| file.serial().map(|serial| (file.name.as_str(), serial)))
        .collect();

    let snapshot = manifest_snapshot_version(&hars, &origin, &mut findings);
    let reproduced = match &seeds {
        Some(seeds) if has_uploads => reproduce(&groups, seeds, &module, &mut findings),
        _ => Vec::new(),
    };
    let card_firmware = args
        .firmware
        .as_deref()
        .map(parse_card_firmware)
        .transpose()?;

    let service = Service {
        origin: &origin,
        rp_id: &args.rp_id,
        module: &module,
        reproduced: &reproduced,
        serials: &serials,
        pow: pow_params(args),
        bodies: &bodies,
        snapshot,
        kind: args.kind.into(),
        intent,
        card_firmware,
        declarations: &declarations,
        declaration_use: RefCell::new(BTreeSet::new()),
        monochrome_authorization: args.monochrome_authorization.map(Into::into),
        manuals: manuals.as_ref(),
    };

    let mut credentials: Vec<(String, Vec<u8>)> = Vec::new();
    let mut covered = vec![0usize; reproduced.len()];
    let mut parsed: Vec<ParsedUpload> = Vec::new();
    let mut parsed_confirmations: Vec<ParsedConfirmation> = Vec::new();
    for (path, har) in &hars {
        findings.note("");
        findings.note(format!(
            "har: {} ({} requests)",
            path.display(),
            har.log.entries.len()
        ));
        let mut uploads = Vec::new();
        let mut confirmations = Vec::new();
        let mut withdrawals = 0usize;
        for entry in &har.log.entries {
            let class = classify(&entry.request, &origin);
            let marker = match class {
                RequestClass::Unexpected => "[UNEXPECTED]",
                _ => "[expected]",
            };
            findings.note(format!(
                "  {marker} {} {}",
                entry.request.method, entry.request.url
            ));
            match class {
                RequestClass::Unexpected => findings.fail(format!(
                    "unexpected request in {}: {} {}",
                    path.display(),
                    entry.request.method,
                    entry.request.url
                )),
                RequestClass::Upload => uploads.push(entry),
                RequestClass::ManualConfirmation => confirmations.push(entry),
                RequestClass::ManualWithdrawal => withdrawals += 1,
                _ => {}
            }
            if UrlParts::parse(&entry.request.url).is_some_and(|parts| parts.origin() == origin.0) {
                audit_request(
                    &entry.request,
                    class,
                    &service,
                    &mut findings,
                    &mut credentials,
                );
                if class == RequestClass::SessionStatus {
                    audit_session_status(&entry.request, har, &origin, &mut findings);
                }
            }
        }
        if uploads.is_empty() && confirmations.is_empty() && withdrawals == 0 {
            findings.fail(format!(
                "{} has no POST {UPLOAD_PATH}, {MANUAL_CONFIRMATION_PATH} or {MANUAL_WITHDRAWAL_PATH} request",
                path.display()
            ));
        }
        for (index, entry) in uploads.iter().enumerate() {
            findings.note("");
            findings.note(format!("upload {} of {}", index + 1, file_name(path)));
            if let Some(upload) = parse_upload(&entry.request, &mut findings) {
                parsed.push(upload);
            }
        }
        for entry in confirmations {
            if let Some(confirmation) = parse_confirmation(&entry.request, &mut findings) {
                parsed_confirmations.push(confirmation);
            }
        }
    }

    let mut sends: BTreeMap<[u8; 32], Vec<usize>> = BTreeMap::new();
    for (index, upload) in parsed.iter().enumerate() {
        sends
            .entry(*upload.envelope.nonce.as_bytes())
            .or_default()
            .push(index);
    }
    for (nonce, indices) in &sends {
        let label = format!("send {}", hex(&nonce[..4]));
        findings.note("");
        findings.note(format!("{label}: {} batch(es)", indices.len()));
        verify_send(
            &parsed,
            indices,
            &service,
            &label,
            &mut findings,
            &mut covered,
            &mut credentials,
        );
    }
    verify_confirmations(
        &parsed,
        &sends,
        &parsed_confirmations,
        &service,
        &mut findings,
        &mut credentials,
    );

    if service.monochrome_authorization.is_some()
        && !parsed
            .iter()
            .any(|upload| upload.envelope.manifest.render_class == RenderClass::Monochrome)
    {
        findings.fail(
            "--monochrome-authorization was declared, but no upload names a monochrome batch",
        );
    }
    for key in service.declarations.keys() {
        if !service.declaration_use.borrow().contains(key) {
            findings.fail(format!(
                "--declare-setting {key} is not applicable to any uploaded batch"
            ));
        }
    }

    if findings.failures.is_empty() && !parsed.is_empty() {
        for (index, count) in covered.iter().enumerate() {
            match count {
                1 => {}
                0 => findings.fail(format!(
                    "no upload names the reproduced image {index} (blob {}); the HAR does not cover every prepared image",
                    hex(&reproduced[index].blob_sha256)
                )),
                _ => findings.fail(format!(
                    "the reproduced image {index} (blob {}) appears in {count} uploads; each prepared image belongs to one batch",
                    hex(&reproduced[index].blob_sha256)
                )),
            }
        }
    }

    record_credential(
        &mut state,
        &credentials,
        &state_path,
        &origin,
        &args.rp_id,
        &mut findings,
    )?;

    Ok(findings)
}

fn intent_before_files(args: &VerifyPayloadArgs) -> Result<Option<Intent>> {
    match args.intent {
        DeclaredIntent::Card => {
            if args.target_look.is_some() {
                bail!("--target-look is only meaningful with --intent recipe");
            }
            Ok(None)
        }
        DeclaredIntent::Native | DeclaredIntent::Recipe => {
            if args.firmware.is_some() {
                bail!("--firmware is only meaningful with --intent card");
            }
            Ok(Some(declared_intent(
                args.intent,
                args.target_look.as_deref(),
            )?))
        }
    }
}

fn load_manual_catalog(data: &Path) -> Result<ManualCatalog> {
    let root = data.join("manuals");
    if !root.is_dir() {
        bail!(
            "{} is not a directory; a manual confirmation withdrawal needs the local documented manual catalog",
            root.display()
        );
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&root).with_context(|| format!("reading {}", root.display()))? {
        let entry = entry.with_context(|| format!("reading {}", root.display()))?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("yaml") {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        files.push((entry.file_name().to_string_lossy().into_owned(), source));
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    if files.is_empty() {
        bail!(
            "{} contains no manual catalog YAML; a manual confirmation withdrawal needs the local documented manual catalog",
            root.display()
        );
    }
    let catalogs = files
        .iter()
        .map(|(name, source)| {
            validate_manual_catalog(source).map_err(|errors| {
                anyhow!(
                    "{}/{name}: {}",
                    root.display(),
                    validation_error_text(&errors)
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    merge_manual_catalogs(catalogs)
        .map_err(|errors| anyhow!("{}: {}", root.display(), validation_error_text(&errors)))
}

fn validation_error_text(errors: &[looklab_schema::ValidationError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

fn parse_declared_settings(args: &[String]) -> Result<BTreeMap<ControlKey, StepIndex>> {
    let mut declarations = BTreeMap::new();
    for text in args {
        let (key, step) = text
            .split_once('=')
            .with_context(|| format!("--declare-setting {text:?} is not <key>=<step>"))?;
        let key = ControlKey::new(key.trim())
            .with_context(|| format!("--declare-setting {text:?}: invalid control key"))?;
        let step: i64 = step
            .trim()
            .parse()
            .with_context(|| format!("--declare-setting {text:?}: the step is not an integer"))?;
        if declarations
            .insert(key.clone(), StepIndex::new(step))
            .is_some()
        {
            bail!("--declare-setting names {key} twice");
        }
    }
    Ok(declarations)
}

/// The community snapshot the uploads name. One run may only name one.
fn manifest_snapshot_version(
    hars: &[(PathBuf, Har)],
    origin: &Origin,
    findings: &mut Findings,
) -> CommunitySnapshotVersion {
    let mut named = None;
    for (_, har) in hars {
        for entry in &har.log.entries {
            if classify(&entry.request, origin) != RequestClass::Upload {
                continue;
            }
            let Ok(body) = entry.request.body() else {
                continue;
            };
            let Ok(envelope) = parse_envelope_strict(&body, &Limits::default()) else {
                continue;
            };
            let version = envelope.manifest.community_snapshot_version;
            match named {
                None => named = Some(version),
                Some(existing) if existing != version => findings.fail(format!(
                    "the uploads name different community snapshots ({existing} and {version})"
                )),
                Some(_) => {}
            }
        }
    }
    named.unwrap_or(CommunitySnapshotVersion::SHIPPED)
}

/// The declared fields one upload names: the kind, the declared intent, and
/// for intent `card` the declaration its images carry.
fn upload_declared(
    service: &Service<'_>,
    images: &[&ReproducedImage],
    findings: &mut Findings,
) -> Option<DeclaredFields> {
    let intent = match &service.intent {
        Some(intent) => intent.clone(),
        None => card_intent(images, service.card_firmware.as_ref(), findings)?,
    };
    Some(DeclaredFields {
        kind: service.kind,
        intent,
    })
}

fn card_intent(
    images: &[&ReproducedImage],
    declared: Option<&(FirmwareId, FirmwareVersion)>,
    findings: &mut Findings,
) -> Option<Intent> {
    let mut tags = images
        .iter()
        .filter_map(|image| image.metadata.render_tag.clone());
    let first = tags.next();
    if tags.any(|tag| Some(&tag) != first.as_ref()) {
        findings.fail("the upload's images disagree on the render tag; one batch carries one tag");
        return None;
    }
    match (first, declared) {
        (Some(tag), declared) => {
            if let Some((id, version)) = declared
                && (&tag.firmware_id != id || &tag.firmware_version != version)
            {
                findings.fail(format!(
                    "--firmware {id}/{version} disagrees with the render tag {}/{}",
                    tag.firmware_id, tag.firmware_version
                ));
            }
            Some(Intent::Card(CardDeclaration::Tagged(tag)))
        }
        (None, Some((id, version))) => Some(Intent::Card(CardDeclaration::user_declared(
            id.clone(),
            version.clone(),
        ))),
        (None, None) => {
            findings.fail(
                "the upload's intent is undetermined: --intent card needs --firmware <id>/<version> when no image carries a render tag",
            );
            None
        }
    }
}

fn parse_card_firmware(text: &str) -> Result<(FirmwareId, FirmwareVersion)> {
    let (id, version) = text
        .split_once('/')
        .with_context(|| format!("--firmware {text:?} is not <firmware-id>/<firmware-version>"))?;
    if version.contains('/') {
        bail!("--firmware {text:?} is not <firmware-id>/<firmware-version>");
    }
    let id = FirmwareId::new(id).map_err(|error| anyhow!("--firmware id {id:?}: {error}"))?;
    let version =
        FirmwareVersion::new(version).map_err(|error| anyhow!("--firmware version: {error}"))?;
    Ok((id, version))
}

fn declared_intent(intent: DeclaredIntent, target_look: Option<&str>) -> Result<Intent> {
    match intent {
        DeclaredIntent::Native => {
            if target_look.is_some() {
                bail!("--target-look is only meaningful with --intent recipe");
            }
            Ok(Intent::Native)
        }
        DeclaredIntent::Recipe => {
            let slug = target_look.context("--intent recipe needs --target-look")?;
            let target_look = LookSlug::new(slug)
                .with_context(|| format!("--target-look {slug:?} is not a look slug"))?;
            Ok(Intent::Recipe { target_look })
        }
        DeclaredIntent::Card => bail!("a card intent is resolved from the files"),
    }
}

#[derive(Debug, Clone, PartialEq)]
struct DeclaredFields {
    kind: ContributionKind,
    intent: Intent,
}

/// One control value in the batch settings.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BatchSetting {
    key: ControlKey,
    step: StepIndex,
    source: SettingSource,
    /// The makernote tag a reproduced value came from; empty when declared.
    tag: &'static str,
}

/// The batch fields the files determine, with each setting's source.
#[derive(Debug, Clone, PartialEq)]
struct BatchFields {
    version: CommunitySnapshotVersion,
    body: BodyId,
    firmware: FirmwareVersionString,
    sim: SimKey,
    render_class: RenderClass,
    family: LookId,
    mono_filter: Option<MonoFilter>,
    settings: ControlSettings,
    settings_sources: Vec<BatchSetting>,
}

fn derive_batch(
    images: &[&ReproducedImage],
    bodies: &Bodies,
    version: CommunitySnapshotVersion,
    declarations: &BTreeMap<ControlKey, StepIndex>,
    card: bool,
    card_family: Option<&LookId>,
    card_render_class: Option<RenderClass>,
    used: &RefCell<BTreeSet<ControlKey>>,
    findings: &mut Findings,
) -> Option<BatchFields> {
    let records = match bodies.at(version) {
        Ok(records) => records,
        Err(error) => {
            findings.fail(format!(
                "the manifest names community snapshot {version}: {error}; --data has up to {}",
                bodies.current_version()
            ));
            return None;
        }
    };
    let first = images.first()?;
    let fields = match derive_one(
        &first.metadata,
        records,
        version,
        declarations,
        card,
        card_family,
        card_render_class,
        used,
    ) {
        Ok(fields) => fields,
        Err(reason) => {
            findings.fail(format!(
                "the files do not determine the batch fields: {reason}"
            ));
            return None;
        }
    };
    for image in &images[1..] {
        match derive_one(
            &image.metadata,
            records,
            version,
            declarations,
            card,
            card_family,
            card_render_class,
            used,
        ) {
            Ok(other) if other == fields => {}
            Ok(_) => {
                findings.fail(
                    "the images disagree on the batch fields; they must come from one body and one camera setting",
                );
                return None;
            }
            Err(reason) => {
                findings.fail(format!(
                    "the files do not determine the batch fields: {reason}"
                ));
                return None;
            }
        }
    }
    Some(fields)
}

fn derive_one(
    metadata: &ManifestMetadata,
    records: &BTreeMap<String, BodyRecord>,
    version: CommunitySnapshotVersion,
    declarations: &BTreeMap<ControlKey, StepIndex>,
    card: bool,
    card_family: Option<&LookId>,
    card_render_class: Option<RenderClass>,
    used: &RefCell<BTreeSet<ControlKey>>,
) -> Result<BatchFields, String> {
    let make = metadata
        .make
        .as_deref()
        .ok_or("the image carries no EXIF make")?;
    let model = metadata
        .model
        .as_deref()
        .ok_or("the image carries no EXIF model")?;
    let body = match_body(records, make, model)?;
    let record = records
        .get(body.as_str())
        .ok_or_else(|| format!("snapshot {version} has no control schema for {body}"))?;
    let firmware_text = metadata
        .firmware
        .as_deref()
        .filter(|text| !text.is_empty())
        .ok_or("the image carries no EXIF firmware")?;
    let firmware = FirmwareVersionString::new(firmware_text)
        .map_err(|error| format!("EXIF firmware {firmware_text:?}: {error}"))?;
    let epoch = resolve_epoch(&record.schema, firmware_text)
        .ok_or_else(|| format!("no epoch covers firmware {firmware_text}"))?;
    let values = MakernoteValues::from(metadata);
    let native = classify_render(epoch, &values);
    let native_sim = match &native {
        Ok(classification) => classification.native_sim.clone(),
        Err(_) => sim_from_makernote(epoch, &values)
            .map_err(|error| format!("the files do not determine the film simulation: {error}"))?,
    };
    let (render_class, family, mono_filter) = match (card, card_family, card_render_class) {
        (true, Some(family), Some(declared)) => {
            let declared = match declared {
                RenderClass::Colour => SchemaRenderClass::Colour,
                RenderClass::Monochrome => SchemaRenderClass::Monochrome,
            };
            let classified = classify_card_render(epoch, family, declared, &values)
                .map_err(|error| format!("the card render does not classify: {error}"))?;
            (
                schema_render_class(classified.render_class),
                classified.family,
                classified.mono_filter,
            )
        }
        _ => {
            let classification = native
                .map_err(|error| format!("the files do not determine the render class: {error}"))?;
            (
                schema_render_class(classification.render_class),
                classification.family,
                classification.mono_filter,
            )
        }
    };
    if render_class == RenderClass::Monochrome && !epoch.supports_monochrome_intake(&family) {
        return Err(format!(
            "the schema does not accept monochrome intake for family {family}"
        ));
    }
    let reach = applicable_controls(epoch, &family);
    let (derived, problems) =
        settings_from_makernote_for(&record.schema, epoch.id, &family, &values);
    let mut sources: Vec<BatchSetting> = derived
        .entries()
        .iter()
        .map(|setting| BatchSetting {
            key: setting.key.clone(),
            step: StepIndex::new(setting.step),
            source: SettingSource::Reproduced,
            tag: setting.source.name(),
        })
        .collect();
    for (key, step) in declarations {
        let applicable = reach.mapped.contains(key)
            || reach.unmapped.contains(key)
            || reach.spatial.contains(key);
        if !applicable {
            continue;
        }
        if sources.iter().any(|setting| &setting.key == key) {
            continue;
        }
        used.borrow_mut().insert(key.clone());
        sources.push(BatchSetting {
            key: key.clone(),
            step: *step,
            source: SettingSource::Declared,
            tag: "",
        });
    }
    sources.sort_by(|left, right| left.key.cmp(&right.key));
    let deferred = render_class == RenderClass::Monochrome;
    let mut required: Vec<&ControlKey> = reach.mapped.iter().chain(&reach.unmapped).collect();
    if deferred {
        required.extend(reach.spatial.iter());
    }
    required.sort_by(|left, right| left.cmp(right));
    required.dedup();
    if !card || deferred {
        for missing in required {
            if sources.iter().any(|setting| &setting.key == missing) {
                continue;
            }
            let detail = problems.iter().find_map(|problem| match problem {
                MappingError::MissingTag { control, .. }
                | MappingError::UnmappedValue { control, .. }
                | MappingError::ScaleNotNumeric { control, .. }
                | MappingError::NotOnStep { control, .. }
                    if control.as_str() == missing.as_str() =>
                {
                    Some(problem.to_string())
                }
                _ => None,
            });
            let reason = if reach.mapped.contains(missing) {
                "the makernote did not determine it"
            } else if reach.spatial.contains(missing) {
                "a deferred batch keeps its spatial settings"
            } else {
                "this snapshot maps no makernote tag for it"
            };
            let detail = detail
                .map(|detail| format!(" ({detail})"))
                .unwrap_or_default();
            return Err(format!(
                "control {missing} has no value: {reason}{detail}; declare it with --declare-setting {missing}=<step>",
            ));
        }
    }
    let entries: Vec<ControlEntry> = sources
        .iter()
        .map(|setting| ControlEntry::new(setting.key.clone(), setting.step, setting.source))
        .collect();
    let settings = ControlSettings::new(entries).map_err(|error| error.to_string())?;
    let validation_family = if card && card_family.is_none() {
        None
    } else {
        Some(family.clone())
    };
    validate_settings(
        record,
        firmware_text,
        &native_sim,
        validation_family.as_ref(),
        &settings,
        card,
        deferred,
    )
    .map_err(|problem| format!("the derived settings are not valid for {body}: {problem}"))?;

    Ok(BatchFields {
        version,
        body,
        firmware,
        sim: native_sim,
        render_class,
        family,
        mono_filter,
        settings,
        settings_sources: sources,
    })
}

fn schema_render_class(render_class: SchemaRenderClass) -> RenderClass {
    match render_class {
        SchemaRenderClass::Colour => RenderClass::Colour,
        SchemaRenderClass::Monochrome => RenderClass::Monochrome,
    }
}

fn match_body(
    records: &BTreeMap<String, BodyRecord>,
    make: &str,
    model: &str,
) -> Result<BodyId, String> {
    let make = make.trim();
    let model = model.trim();
    let mut matches = Vec::new();
    for (id, record) in records {
        if record.make.eq_ignore_ascii_case(make)
            && (record.model.eq_ignore_ascii_case(model)
                || record
                    .exif_aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(model)))
        {
            matches.push(id);
        }
    }
    match matches.as_slice() {
        [] => Err(format!(
            "no body in the data snapshot matches EXIF make {make:?} and model {model:?}; pass --data with the snapshot that contains it"
        )),
        [id] => BodyId::new(id).map_err(|error| error.to_string()),
        _ => Err(format!(
            "several bodies in the data snapshot match EXIF make {make:?} and model {model:?}: {matches:?}"
        )),
    }
}

fn print_batch_fields(
    derived: Option<&BatchFields>,
    declared: &DeclaredFields,
    bodies: &Bodies,
    findings: &mut Findings,
) {
    findings.note("");
    findings.note("batch fields");
    match derived {
        Some(fields) => {
            findings.note(format!(
                "  community snapshot: {} (named by the manifest; --data has up to {})",
                fields.version,
                bodies.current_version()
            ));
            findings.note(format!(
                "  body: {} (reproduced from EXIF make/model)",
                fields.body
            ));
            findings.note(format!(
                "  firmware: {} (reproduced from EXIF)",
                fields.firmware
            ));
            findings.note(format!(
                "  sim: {} (reproduced from the {FILM_MODE_TAG} makernote)",
                fields.sim
            ));
            findings.note(format!(
                "  render class: {:?} (reproduced from the schema and makernotes)",
                fields.render_class
            ));
            findings.note(format!("  family: {} (schema family)", fields.family));
            if let Some(filter) = fields.mono_filter {
                findings.note(format!("  mono filter: {filter:?} (reproduced)"));
            }
            let reproduced = fields
                .settings_sources
                .iter()
                .filter(|setting| setting.source == SettingSource::Reproduced)
                .count();
            let declared_count = fields.settings_sources.len() - reproduced;
            findings.note(format!(
                "  settings: {} controls ({reproduced} reproduced, {declared_count} declared)",
                fields.settings_sources.len()
            ));
            for setting in &fields.settings_sources {
                let origin = match setting.source {
                    SettingSource::Reproduced => format!("reproduced from {}", setting.tag),
                    SettingSource::Declared => "declared".to_owned(),
                };
                findings.note(format!(
                    "    {} = {} ({origin})",
                    setting.key,
                    setting.step.get()
                ));
            }
        }
        None => findings
            .note("  community snapshot, body, firmware, sim, settings: not reproduced (see FAIL)"),
    }
    findings.note(format!("  kind: {:?} (declared)", declared.kind));
    match &declared.intent {
        Intent::Native => findings.note("  intent: native (declared)"),
        Intent::Recipe { target_look } => findings.note(format!(
            "  intent: recipe (declared), target look: {target_look} (declared)"
        )),
        Intent::Card(declaration) => match declaration {
            CardDeclaration::Tagged(tag) => {
                findings.note("  intent: card (reproduced from the render tag)");
                findings.note(format!(
                    "  card firmware: {}/{} (reproduced from the render tag)",
                    tag.firmware_id, tag.firmware_version
                ));
                findings.note(format!(
                    "  card look: {}@{} (reproduced from the render tag)",
                    tag.look_slug, tag.look_version
                ));
                findings.note(format!(
                    "  card cid: {} (reproduced from the render tag)",
                    tag.cid_prefix_hex()
                ));
            }
            CardDeclaration::UserDeclared {
                firmware_id,
                firmware_version,
            } => {
                findings.note("  intent: card (declared)");
                findings.note(format!(
                    "  card firmware: {firmware_id}/{firmware_version} (declared)"
                ));
            }
        },
    }
}

fn expected_manifest(
    fields: &BatchFields,
    declared: &DeclaredFields,
    images: &[&ReproducedImage],
    authorization: Option<MonochromeAuthorization>,
) -> BatchManifest {
    BatchManifest {
        kind: declared.kind,
        intent: declared.intent.clone(),
        render_class: fields.render_class,
        monochrome_authorization: match fields.render_class {
            RenderClass::Colour => None,
            RenderClass::Monochrome => authorization,
        },
        body: fields.body.clone(),
        firmware_version: fields.firmware.clone(),
        sim_key: fields.sim.clone(),
        community_snapshot_version: fields.version,
        settings: fields.settings.clone(),
        images: images
            .iter()
            .map(|image| ManifestImage {
                meta: image.metadata.clone(),
                rerender_group: image
                    .rerender_group
                    .map(looklab_upload::RerenderGroupId::new),
                blob_sha256: image.blob_sha256,
                blob_kind: BlobKind::Samples,
            })
            .collect(),
    }
}

struct Service<'a> {
    origin: &'a Origin,
    rp_id: &'a str,
    module: &'a ModuleHost,
    reproduced: &'a [ReproducedImage],
    serials: &'a [(&'a str, &'a UploadSerial)],
    pow: PowParams,
    bodies: &'a Bodies,
    snapshot: CommunitySnapshotVersion,
    kind: ContributionKind,
    /// The declared intent, absent when `--intent card` resolves it per upload.
    intent: Option<Intent>,
    card_firmware: Option<(FirmwareId, FirmwareVersion)>,
    declarations: &'a BTreeMap<ControlKey, StepIndex>,
    /// Declarations applicable to at least one uploaded batch.
    declaration_use: RefCell<BTreeSet<ControlKey>>,
    monochrome_authorization: Option<MonochromeAuthorization>,
    manuals: Option<&'a ManualCatalog>,
}

#[derive(Debug)]
struct ReproducedImage {
    metadata: ManifestMetadata,
    rerender_group: Option<[u8; 16]>,
    blob: Vec<u8>,
    blob_sha256: [u8; 32],
}

struct ParsedUpload {
    envelope: Envelope,
    body: Vec<u8>,
}

fn parse_upload(request: &HarRequest, findings: &mut Findings) -> Option<ParsedUpload> {
    if let Some(mime_type) = request
        .post_data
        .as_ref()
        .and_then(|post| post.mime_type.as_deref())
        && !mime_type.starts_with("application/octet-stream")
    {
        findings.fail(format!(
            "the upload request's content type is {mime_type:?}, expected application/octet-stream"
        ));
    }
    let body = match request.body() {
        Ok(body) => body,
        Err(error) => {
            findings.fail(format!("the upload body could not be extracted: {error}"));
            return None;
        }
    };
    let envelope = match parse_envelope_strict(&body, &Limits::default()) {
        Ok(envelope) => envelope,
        Err(error) => {
            findings.fail(format!(
                "the upload body is not the canonical envelope: {error}"
            ));
            return None;
        }
    };
    findings.note(format!(
        "  envelope: canonical LLUPLD version {}, {} bytes, no trailing bytes",
        looklab_upload::VERSION,
        body.len()
    ));
    findings.note(format!(
        "  nonce: {} bytes",
        envelope.nonce.as_bytes().len()
    ));
    findings.note(format!(
        "  send manifest: {} batch(es), {} attestation(s)",
        envelope.send_manifest.batches.len(),
        envelope.send_manifest.attestations.len()
    ));
    Some(ParsedUpload { envelope, body })
}

fn verify_send(
    parsed: &[ParsedUpload],
    indices: &[usize],
    service: &Service<'_>,
    label: &str,
    findings: &mut Findings,
    covered: &mut [usize],
    credentials: &mut Vec<(String, Vec<u8>)>,
) {
    let first = &parsed[indices[0]].envelope;
    for &index in &indices[1..] {
        let other = &parsed[index].envelope;
        if other.send_manifest_bytes != first.send_manifest_bytes {
            findings.fail(format!(
                "{label}: the send's batches carry different send manifests"
            ));
        }
        if other.assertion != first.assertion {
            findings.fail(format!(
                "{label}: the send's batches carry different assertions; the browser approved more than once"
            ));
        }
    }

    let mut present: Vec<ManifestHash> = indices
        .iter()
        .map(|&index| parsed[index].envelope.manifest_hash())
        .collect();
    present.sort_unstable();
    if present != first.send_manifest.batches {
        let listed: BTreeSet<ManifestHash> = first.send_manifest.batches.iter().copied().collect();
        let found: BTreeSet<ManifestHash> = present.iter().copied().collect();
        for hash in listed.difference(&found) {
            findings.fail(format!(
                "{label}: batch {} is missing from the HAR",
                hex(hash.as_bytes())
            ));
        }
        for hash in found.difference(&listed) {
            findings.fail(format!(
                "{label}: batch {} is not in the send manifest",
                hex(hash.as_bytes())
            ));
        }
        if present.len() != found.len() {
            findings.fail(format!(
                "{label}: a batch appears more than once in the HAR"
            ));
        } else if present.len() != listed.len() && found == listed {
            findings.fail(format!(
                "{label}: the HAR carries {} batches but the send manifest lists {}",
                present.len(),
                listed.len()
            ));
        }
    }

    findings.note(format!("  one assertion over {} batch(es)", indices.len()));
    check_assertion(
        &first.assertion,
        service,
        first.challenge(),
        "H(nonce ‖ send manifest hash)",
        findings,
    );
    for &index in indices {
        verify_upload(&parsed[index], service, findings, covered);
    }
    credentials.push((label.to_owned(), first.assertion.credential_id.clone()));
}

struct ParsedConfirmation {
    nonce: Nonce,
    send_manifest: SendManifest,
    send_manifest_bytes: Vec<u8>,
    assertion: WebAuthnAssertion,
    digest: Sha256Hex,
}

fn decode_confirmation_field(value: &str, max: usize) -> Option<Vec<u8>> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .ok()?;
    (!bytes.is_empty() && bytes.len() <= max).then_some(bytes)
}

fn escaped(value: &str) -> String {
    format!("{value:?}")
}

fn print_manual_confirmation(
    confirmation: &looklab_schema::ManualConfirmation,
    findings: &mut Findings,
) {
    findings.note("  manual confirmation, owner-declared (requires human review):");
    findings.note(format!("    version: {}", confirmation.version));
    findings.note(format!(
        "    camera: {}",
        escaped(confirmation.camera.as_str())
    ));
    findings.note(format!(
        "    camera definition digest: {}",
        hex(confirmation.camera_digest.as_bytes())
    ));
    findings.note(format!(
        "    reported firmware: {}",
        escaped(confirmation.firmware.as_str())
    ));
    match &confirmation.scope {
        SchemaConfirmationScope::Complete => findings.note("    scope: complete".to_owned()),
        SchemaConfirmationScope::Controls { keys } => {
            findings.note(format!(
                "    scope: controls, {} key(s) in payload order:",
                keys.len()
            ));
            for (index, key) in keys.iter().enumerate() {
                findings.note(format!(
                    "      scope key [{index}]: {}",
                    escaped(key.as_str())
                ));
            }
        }
    }
    findings.note(format!(
        "    observations: {} in payload order:",
        confirmation.observations.len()
    ));
    for (index, observation) in confirmation.observations.iter().enumerate() {
        findings.note(format!("      observation [{index}]:"));
        findings.note(format!(
            "        key: {}",
            escaped(observation.key.as_str())
        ));
        findings.note(format!(
            "        definition digest: {}",
            hex(observation.definition_digest.as_bytes())
        ));
        match &observation.finding {
            SchemaFinding::Matches => {
                findings.note("        finding: matches the documented options".to_owned());
            }
            SchemaFinding::Corrected {
                availability,
                domain,
            } => {
                findings.note(format!(
                    "        finding: corrected, availability {availability}"
                ));
                findings.note(format!("        correction domain: {domain:?}"));
            }
        }
    }
    findings.review(format!(
        "manual confirmation {} is owner-declared: its firmware, scope, observations and correction domains are bound by the send's signed attestation but not reproduced from photographs; review the printed declarations",
        hex(confirmation.digest().as_bytes())
    ));
}

fn parse_confirmation(request: &HarRequest, findings: &mut Findings) -> Option<ParsedConfirmation> {
    if let Some(mime_type) = request
        .post_data
        .as_ref()
        .and_then(|post| post.mime_type.as_deref())
        && !mime_type.starts_with("application/json")
    {
        findings.fail(format!(
            "the manual confirmation's content type is {mime_type:?}, expected application/json"
        ));
    }
    let raw = match request.body() {
        Ok(body) => body,
        Err(error) => {
            findings.fail(format!(
                "the manual confirmation body could not be extracted: {error}"
            ));
            return None;
        }
    };
    let body: looklab_upload::ManualConfirmationRequest = match serde_json::from_slice(&raw) {
        Ok(body) => body,
        Err(error) => {
            findings.fail(format!(
                "the manual confirmation is not the canonical request object: {error}"
            ));
            return None;
        }
    };
    if body.canonical_bytes() != raw {
        findings.fail(
            "the manual confirmation request is not the canonical encoding: whitespace, escapes or field order differ from the builder's bytes",
        );
        return None;
    }
    if body.confirmation.canonical_bytes().len() > MAX_CONFIRMATION_BYTES {
        findings.fail("the manual confirmation payload is larger than the allowed maximum");
        return None;
    }
    if let Err(errors) = body.confirmation.validate_structure() {
        for error in &errors {
            findings.fail(format!(
                "the manual confirmation is structurally invalid: {error}"
            ));
        }
        return None;
    }
    print_manual_confirmation(&body.confirmation, findings);
    let Some(nonce_bytes) = decode_confirmation_field(&body.nonce, 32) else {
        findings.fail("the manual confirmation's nonce is not 32 base64url bytes");
        return None;
    };
    let Some(nonce) = Nonce::from_slice(&nonce_bytes) else {
        findings.fail("the manual confirmation's nonce is not 32 bytes");
        return None;
    };
    let Some(credential_id) = decode_confirmation_field(&body.credential_id, 255) else {
        findings.fail("the manual confirmation's credential id is not base64url within 255 bytes");
        return None;
    };
    let Some(authenticator_data) = decode_confirmation_field(&body.authenticator_data, 512) else {
        findings
            .fail("the manual confirmation's authenticator data is not base64url within 512 bytes");
        return None;
    };
    let Some(client_data_json) = decode_confirmation_field(&body.client_data_json, 1024) else {
        findings.fail("the manual confirmation's client data is not base64url within 1024 bytes");
        return None;
    };
    let Some(signature) = decode_confirmation_field(&body.signature, 512) else {
        findings.fail("the manual confirmation's signature is not base64url within 512 bytes");
        return None;
    };
    let Some(manifest_bytes) = decode_confirmation_field(&body.send_manifest, 1024 * 1024) else {
        findings.fail("the manual confirmation's send manifest is not base64url within 1 MiB");
        return None;
    };
    let (send_manifest, rest) = match postcard::take_from_bytes::<SendManifest>(&manifest_bytes) {
        Ok(value) => value,
        Err(error) => {
            findings.fail(format!(
                "the manual confirmation's send manifest is not postcard: {error}"
            ));
            return None;
        }
    };
    if !rest.is_empty() || canonical_send_manifest_bytes(&send_manifest) != manifest_bytes {
        findings.fail("the manual confirmation's send manifest is not canonical");
        return None;
    }
    if let Err(error) = send_manifest.validate(DEFAULT_MAX_BATCHES, DEFAULT_MAX_ATTESTATIONS) {
        findings.fail(format!(
            "the manual confirmation's send manifest is invalid: {error}"
        ));
        return None;
    }
    let digest = body.confirmation.digest();
    findings.note(format!(
        "  manual confirmation: {} batch(es), {} attestation(s), payload {}",
        send_manifest.batches.len(),
        send_manifest.attestations.len(),
        hex(digest.as_bytes())
    ));
    Some(ParsedConfirmation {
        nonce,
        send_manifest,
        send_manifest_bytes: manifest_bytes,
        assertion: WebAuthnAssertion {
            credential_id,
            authenticator_data,
            client_data_json,
            signature,
        },
        digest,
    })
}

fn audit_withdrawal(
    request: &HarRequest,
    service: &Service<'_>,
    findings: &mut Findings,
    credentials: &mut Vec<(String, Vec<u8>)>,
) {
    let raw = match request.body() {
        Ok(body) => body,
        Err(error) => {
            findings.fail(format!(
                "the manual confirmation withdrawal body could not be extracted: {error}"
            ));
            return;
        }
    };
    let body: ManualWithdrawalRequest = match serde_json::from_slice(&raw) {
        Ok(body) => body,
        Err(error) => {
            findings.fail(format!(
                "the manual confirmation withdrawal is not the canonical request object: {error}"
            ));
            return;
        }
    };
    if body.canonical_bytes() != raw {
        findings.fail(
            "the manual confirmation withdrawal is not the canonical encoding: whitespace, escapes or field order differ from the builder's bytes",
        );
        return;
    }
    let Some(manuals) = service.manuals else {
        findings.fail(
            "the manual confirmation withdrawal has no local documented manual catalog to resolve its camera",
        );
        return;
    };
    let Some(camera) = manuals.camera(&body.camera) else {
        findings.fail(format!(
            "the manual confirmation withdrawal names camera {}, which is not in the local documented manual catalog",
            escaped(body.camera.as_str())
        ));
        return;
    };
    findings.note(format!(
        "  withdrawal target: {} ({} {})",
        escaped(camera.id.as_str()),
        escaped(&camera.make),
        escaped(&camera.model)
    ));
    let Some(nonce) =
        decode_confirmation_field(&body.nonce, 32).and_then(|bytes| Nonce::from_slice(&bytes))
    else {
        findings.fail("the manual confirmation withdrawal's nonce is not 32 base64url bytes");
        return;
    };
    let Some(credential_id) = decode_confirmation_field(&body.credential_id, 255) else {
        findings.fail(
            "the manual confirmation withdrawal's credential id is not base64url within 255 bytes",
        );
        return;
    };
    let Some(authenticator_data) = decode_confirmation_field(&body.authenticator_data, 512) else {
        findings.fail(
            "the manual confirmation withdrawal's authenticator data is not base64url within 512 bytes",
        );
        return;
    };
    let Some(client_data_json) = decode_confirmation_field(&body.client_data_json, 1024) else {
        findings.fail(
            "the manual confirmation withdrawal's client data is not base64url within 1024 bytes",
        );
        return;
    };
    let Some(signature) = decode_confirmation_field(&body.signature, 512) else {
        findings.fail(
            "the manual confirmation withdrawal's signature is not base64url within 512 bytes",
        );
        return;
    };
    let assertion = WebAuthnAssertion {
        credential_id,
        authenticator_data,
        client_data_json,
        signature,
    };
    let digest = request_digest(
        RequestDomain::WithdrawManualConfirmation,
        body.camera.as_str().as_bytes(),
    );
    check_assertion(
        &assertion,
        service,
        Challenge::new(&nonce, &digest),
        "H(nonce ‖ H(\"withdraw-manual-confirmation\" ‖ camera))",
        findings,
    );
    credentials.push((
        "manual confirmation withdrawal".to_owned(),
        assertion.credential_id.clone(),
    ));
}

fn verify_confirmations(
    parsed: &[ParsedUpload],
    sends: &BTreeMap<[u8; 32], Vec<usize>>,
    confirmations: &[ParsedConfirmation],
    service: &Service<'_>,
    findings: &mut Findings,
    credentials: &mut Vec<(String, Vec<u8>)>,
) {
    let mut by_send: BTreeMap<[u8; 32], Vec<&ParsedConfirmation>> = BTreeMap::new();
    for confirmation in confirmations {
        by_send
            .entry(*confirmation.nonce.as_bytes())
            .or_default()
            .push(confirmation);
    }
    let mut nonces: BTreeSet<[u8; 32]> = sends.keys().copied().collect();
    nonces.extend(by_send.keys().copied());
    for nonce in nonces {
        let nonce = Nonce::from_bytes(nonce);
        let list = by_send
            .get(nonce.as_bytes())
            .map(|list| list.as_slice())
            .unwrap_or(&[]);
        let label = format!("send {}", hex(&nonce.as_bytes()[..4]));
        if !list.is_empty() {
            findings.note("");
            findings.note(format!(
                "{label}: {} manual confirmation request(s)",
                list.len()
            ));
        }
        let manifest: &SendManifest = match sends.get(nonce.as_bytes()) {
            Some(indices) => {
                let upload = &parsed[indices[0]].envelope;
                for confirmation in list {
                    if confirmation.send_manifest_bytes != upload.send_manifest_bytes {
                        findings.fail(format!(
                            "{label}: a manual confirmation carries a different send manifest than the uploads"
                        ));
                    }
                    if confirmation.assertion != upload.assertion {
                        findings.fail(format!(
                            "{label}: a manual confirmation carries a different assertion than the uploads"
                        ));
                    }
                }
                &upload.send_manifest
            }
            None => {
                let Some(first) = list.first() else {
                    continue;
                };
                if !first.send_manifest.batches.is_empty() {
                    findings.fail(format!(
                        "{label}: the send manifest declares {} batch(es) but the HAR has no upload for them",
                        first.send_manifest.batches.len()
                    ));
                }
                for confirmation in list {
                    if confirmation.send_manifest_bytes != first.send_manifest_bytes {
                        findings.fail(format!(
                            "{label}: the send's manual confirmations carry different send manifests"
                        ));
                    }
                    if confirmation.assertion != first.assertion {
                        findings.fail(format!(
                            "{label}: the send's manual confirmations carry different assertions; the browser approved more than once"
                        ));
                    }
                }
                check_assertion(
                    &first.assertion,
                    service,
                    Challenge::for_send(&nonce, &send_manifest_hash(&first.send_manifest)),
                    "H(nonce ‖ send manifest hash)",
                    findings,
                );
                credentials.push((label.clone(), first.assertion.credential_id.clone()));
                &first.send_manifest
            }
        };
        let declared: BTreeSet<[u8; 32]> = manifest
            .attestations
            .iter()
            .map(|hash| *hash.as_bytes())
            .collect();
        let submitted: BTreeSet<[u8; 32]> = list
            .iter()
            .map(|confirmation| *confirmation.digest.as_bytes())
            .collect();
        for digest in declared.difference(&submitted) {
            findings.fail(format!(
                "{label}: the send manifest lists attestation {} but the HAR has no manual confirmation for it",
                hex(digest)
            ));
        }
        for digest in submitted.difference(&declared) {
            findings.fail(format!(
                "{label}: manual confirmation {} is not in the send manifest",
                hex(digest)
            ));
        }
        if submitted.len() != list.len() {
            findings.fail(format!(
                "{label}: a manual confirmation appears more than once in the HAR"
            ));
        }
    }
}

fn verify_upload(
    upload: &ParsedUpload,
    service: &Service<'_>,
    findings: &mut Findings,
    covered: &mut [usize],
) {
    let envelope = &upload.envelope;
    let selected = select_images(envelope, service.reproduced, findings);
    let images = selected.as_ref().map(|indices| {
        indices
            .iter()
            .map(|&index| &service.reproduced[index])
            .collect::<Vec<_>>()
    });
    if let Some(indices) = &selected {
        for &index in indices {
            covered[index] += 1;
            if covered[index] > 1 {
                findings.fail(format!(
                    "the reproduced image {index} appears in more than one upload; each prepared image belongs to one batch"
                ));
            }
        }
    }
    if let Some(images) = &images {
        findings.note(format!(
            "  images: {} of {} reproduced, in the manifest's order",
            images.len(),
            service.reproduced.len()
        ));
        match upload_declared(service, images, findings) {
            Some(declared) => {
                let card_family = match &declared.intent {
                    Intent::Card(CardDeclaration::Tagged(tag)) => {
                        LookId::new(tag.look_slug.as_str()).ok()
                    }
                    _ => None,
                };
                let derived = derive_batch(
                    images,
                    service.bodies,
                    service.snapshot,
                    service.declarations,
                    matches!(declared.intent, Intent::Card(_)),
                    card_family.as_ref(),
                    matches!(declared.intent, Intent::Card(_))
                        .then_some(envelope.manifest.render_class),
                    &service.declaration_use,
                    findings,
                );
                print_batch_fields(derived.as_ref(), &declared, service.bodies, findings);
                if let Some(fields) = &derived {
                    if fields.render_class == RenderClass::Monochrome
                        && service.monochrome_authorization.is_none()
                    {
                        findings.fail(
                            "the upload includes a monochrome batch, but --monochrome-authorization model-development-180-days was not declared",
                        );
                    }
                    let expected = expected_manifest(
                        fields,
                        &declared,
                        images,
                        service.monochrome_authorization,
                    );
                    check_images(envelope, &expected, images, service, findings);
                } else {
                    findings.fail(
                        "the manifest cannot be reproduced: the batch fields are not determined",
                    );
                }
            }
            None => findings.fail(
                "the manifest cannot be reproduced: the upload's declared intent is not determined",
            ),
        }
    } else {
        findings.fail("the manifest cannot be reproduced: the upload's images are not reproduced");
    }

    check_pow(envelope, service, findings);
    check_serial(envelope, service, &upload.body, findings);
}

/// The reproduced images one upload names, in its manifest's order.
fn select_images(
    envelope: &Envelope,
    reproduced: &[ReproducedImage],
    findings: &mut Findings,
) -> Option<Vec<usize>> {
    if envelope.manifest.images.is_empty() {
        findings.fail("the upload names no image");
        return None;
    }
    let mut used = vec![false; reproduced.len()];
    let mut selected = Vec::with_capacity(envelope.manifest.images.len());
    let mut last: Option<usize> = None;
    for (position, recorded) in envelope.manifest.images.iter().enumerate() {
        let matching: Vec<usize> = reproduced
            .iter()
            .enumerate()
            .filter(|(index, image)| !used[*index] && image.matches(recorded))
            .map(|(index, _)| index)
            .collect();
        let Some(&chosen) = matching
            .iter()
            .find(|&&index| last.is_none_or(|last| index > last))
        else {
            if matching.is_empty() {
                if reproduced.iter().any(|image| image.matches(recorded)) {
                    findings.fail(format!(
                        "image {position} repeats a reproduced image; one upload carries each prepared image once"
                    ));
                } else {
                    findings.fail(format!(
                        "image {position} does not match any reproduced image; its blob, metadata or re-render group differs"
                    ));
                }
            } else {
                findings.fail(format!(
                    "image {position} is out of the reproduced order: the files put it before image {}",
                    last.unwrap_or_default()
                ));
            }
            return None;
        };
        used[chosen] = true;
        last = Some(chosen);
        selected.push(chosen);
    }
    Some(selected)
}

impl ReproducedImage {
    fn matches(&self, recorded: &ManifestImage) -> bool {
        self.blob_sha256 == recorded.blob_sha256
            && self.metadata == recorded.meta
            && self
                .rerender_group
                .map(looklab_upload::RerenderGroupId::new)
                == recorded.rerender_group
    }
}

fn check_images(
    envelope: &Envelope,
    expected: &BatchManifest,
    images: &[&ReproducedImage],
    service: &Service<'_>,
    findings: &mut Findings,
) {
    if let Err(error) = service
        .bodies
        .at(envelope.manifest.community_snapshot_version)
    {
        findings.fail(format!(
            "the manifest names community snapshot {}: {error}; --data has up to {}",
            envelope.manifest.community_snapshot_version,
            service.bodies.current_version()
        ));
    }

    let request = BuildManifestRequest {
        kind: expected.kind,
        intent: expected.intent.clone(),
        render_class: expected.render_class,
        monochrome_authorization: expected.monochrome_authorization,
        body: expected.body.clone(),
        firmware: expected.firmware_version.clone(),
        sim: expected.sim_key.clone(),
        community_snapshot_version: expected.community_snapshot_version,
        settings: expected.settings.clone(),
        images: images
            .iter()
            .map(|image| ManifestImageInput {
                metadata: image.metadata.clone(),
                rerender_group: image.rerender_group,
                blob: image.blob.clone(),
                kind: BlobKind::Samples,
            })
            .collect(),
        nonce: envelope.nonce,
    };
    match service.module.build_manifest(&request) {
        Ok(response) => {
            if response.manifest_bytes == envelope.manifest_bytes && envelope.manifest == *expected
            {
                findings.note("  manifest: reproduced and declared fields match, byte for byte");
            } else {
                findings.fail(format!(
                    "the manifest does not match the reproduced and declared manifest ({} bytes, {} expected)",
                    envelope.manifest_bytes.len(),
                    response.manifest_bytes.len()
                ));
                let differences = manifest_differences(&envelope.manifest, expected);
                for difference in differences {
                    findings.fail(format!("manifest: {difference}"));
                }
            }
        }
        Err(error) => {
            findings.fail(format!(
                "the prep module could not rebuild the manifest: {error}"
            ));
        }
    }

    let mut blobs_match = envelope.blobs.len() == images.len();
    for (index, (site_blob, ours)) in envelope.blobs.iter().zip(images).enumerate() {
        if site_blob != &ours.blob {
            blobs_match = false;
            findings.fail(format!(
                "blob {index} differs from the reproduced sample blob"
            ));
        }
    }
    if blobs_match {
        findings.note(format!(
            "  blobs: {} of {} reproduced byte for byte",
            envelope.blobs.len(),
            images.len()
        ));
    }
}

fn manifest_differences(site: &BatchManifest, expected: &BatchManifest) -> Vec<String> {
    let mut differences = Vec::new();
    if site.kind != expected.kind {
        differences.push(format!(
            "kind is {:?}, declared {:?}",
            site.kind, expected.kind
        ));
    }
    if site.intent != expected.intent {
        differences.push(format!(
            "intent is {:?}, declared {:?}",
            site.intent, expected.intent
        ));
    }
    if site.render_class != expected.render_class {
        differences.push(format!(
            "render class is {:?}, reproduced {:?}",
            site.render_class, expected.render_class
        ));
    }
    if site.monochrome_authorization != expected.monochrome_authorization {
        differences.push(format!(
            "monochrome authorization is {:?}, expected {:?}",
            site.monochrome_authorization, expected.monochrome_authorization
        ));
    }
    if site.body != expected.body {
        differences.push(format!(
            "body is {}, reproduced {}",
            site.body, expected.body
        ));
    }
    if site.firmware_version != expected.firmware_version {
        differences.push(format!(
            "firmware version is {}, reproduced {}",
            site.firmware_version, expected.firmware_version
        ));
    }
    if site.sim_key != expected.sim_key {
        differences.push(format!(
            "sim key is {}, reproduced {}",
            site.sim_key, expected.sim_key
        ));
    }
    if site.community_snapshot_version != expected.community_snapshot_version {
        differences.push(format!(
            "community snapshot is {}, the files' snapshot is {}",
            site.community_snapshot_version, expected.community_snapshot_version
        ));
    }
    if site.settings != expected.settings {
        differences.push(format!(
            "control settings are {} entries, reproduced and declared {}",
            site.settings.entries().len(),
            expected.settings.entries().len()
        ));
        for entry in site.settings.entries() {
            let ours = expected
                .settings
                .entries()
                .iter()
                .find(|candidate| candidate.key == entry.key);
            match ours {
                None => differences.push(format!(
                    "setting {} = {} is neither reproduced nor declared",
                    entry.key,
                    entry.step.get()
                )),
                Some(ours) if ours.step != entry.step => {
                    differences.push(format!(
                        "setting {} = {}, expected {}",
                        entry.key,
                        entry.step.get(),
                        ours.step.get()
                    ));
                }
                Some(ours) if ours.source != entry.source => {
                    differences.push(format!(
                        "setting {} is {}, expected {}",
                        entry.key, entry.source, ours.source
                    ));
                }
                Some(_) => {}
            }
        }
        for entry in expected.settings.entries() {
            if !site
                .settings
                .entries()
                .iter()
                .any(|candidate| candidate.key == entry.key)
            {
                differences.push(format!(
                    "setting {} = {} is missing",
                    entry.key,
                    entry.step.get()
                ));
            }
        }
    }
    if site.images.len() != expected.images.len() {
        differences.push(format!(
            "{} image(s), reproduced {}",
            site.images.len(),
            expected.images.len()
        ));
    }
    for (index, (a, b)) in site.images.iter().zip(&expected.images).enumerate() {
        if a.meta != b.meta {
            differences.push(format!("image {index} metadata differs"));
        }
        if a.rerender_group != b.rerender_group {
            differences.push(format!("image {index} re-render group differs"));
        }
        if a.blob_sha256 != b.blob_sha256 {
            differences.push(format!("image {index} blob hash differs"));
        }
        if a.blob_kind != b.blob_kind {
            differences.push(format!("image {index} blob kind differs"));
        }
    }
    differences
}

/// Structural WebAuthn checks against `challenge`; returns whether some supported
/// signature algorithm passed.
fn check_assertion(
    assertion: &WebAuthnAssertion,
    service: &Service<'_>,
    challenge: Challenge,
    challenge_name: &str,
    findings: &mut Findings,
) -> bool {
    let expected = ExpectedAssertion {
        challenge,
        origin: service.origin.0.as_str(),
        rp_id: service.rp_id,
        algorithm: SignatureAlgorithm::Ecdsa,
        top_origin: None,
    };
    let mut failures = BTreeSet::new();
    for algorithm in [
        SignatureAlgorithm::Ecdsa,
        SignatureAlgorithm::Ed25519,
        SignatureAlgorithm::Rsa,
    ] {
        let verdict = check_structure(
            assertion,
            &ExpectedAssertion {
                algorithm,
                ..expected
            },
        );
        match verdict {
            StructuralVerdict::Pass => {
                findings.note(webauthn_line(service, algorithm));
                findings.note(format!(
                    "  challenge: {challenge_name} matches client_data_json"
                ));
                return true;
            }
            StructuralVerdict::ReviewNeeded {
                unknown_client_data_keys,
                non_canonical_client_data,
                top_origin,
                extensions,
            } => {
                findings.note(webauthn_line(service, algorithm));
                findings.note(format!(
                    "  challenge: {challenge_name} matches client_data_json"
                ));
                for extra in unknown_client_data_keys {
                    findings.review(format!(
                        "client_data_json key {:?} appears in full: {}",
                        extra.key, extra.raw_json
                    ));
                }
                if let Some(raw) = non_canonical_client_data {
                    findings.review(format!(
                        "client_data_json is not the WebAuthn serialization, bytes in full: {raw:?}"
                    ));
                }
                match top_origin {
                    Some(TopOriginReview::Unexpected(origin)) => findings.review(format!(
                        "client_data_json topOrigin {:?} has no expected value",
                        origin
                    )),
                    Some(TopOriginReview::Missing) => {
                        findings.review("client_data_json has no expected topOrigin")
                    }
                    None => {}
                }
                if let Some(extensions) = extensions {
                    for entry in extensions.entries {
                        findings.review(format!(
                            "authenticator extension {:?}: {:?}",
                            entry.key, entry.value
                        ));
                    }
                }
                return true;
            }
            StructuralVerdict::Fail(error) => {
                failures.insert(error.to_string());
            }
        }
    }
    findings.fail(format!(
        "webauthn: {}",
        failures.into_iter().collect::<Vec<_>>().join("; ")
    ));
    false
}

fn webauthn_line(service: &Service<'_>, algorithm: SignatureAlgorithm) -> String {
    let signature = match algorithm {
        SignatureAlgorithm::Ecdsa => "ECDSA (DER)",
        SignatureAlgorithm::Ed25519 => "Ed25519 (64 bytes)",
        SignatureAlgorithm::Rsa => "RSA (key size)",
    };
    format!(
        "  webauthn: type webauthn.get, origin {}, crossOrigin false, rpIdHash sha256({}), user present, attested credential data absent, {signature} signature well formed",
        service.origin, service.rp_id
    )
}

fn check_pow(envelope: &Envelope, service: &Service<'_>, findings: &mut Findings) {
    let n_images = u32::try_from(envelope.manifest.images.len()).unwrap_or(u32::MAX);
    match verify_pow(
        &envelope.pow_challenge(),
        n_images,
        &service.pow,
        &envelope.pow,
    ) {
        Ok(()) => {
            let bits = service.pow.difficulty_bits(n_images).unwrap_or(0);
            findings.note(format!(
                "  proof of work: {} of {} sub-puzzles verified at {bits} leading zero bits",
                envelope.pow.iter().count(),
                looklab_upload::POW_K
            ));
        }
        Err(error) => findings.fail(format!("proof of work: {error}")),
    }
}

fn check_serial(envelope: &Envelope, service: &Service<'_>, body: &[u8], findings: &mut Findings) {
    let mut candidates = vec![(None, None)];
    for &(name, serial) in service.serials {
        candidates.push((Some(name), Some(serial)));
    }
    let mut matched = None;
    for (name, serial) in &candidates {
        let parts = EnvelopeParts {
            nonce: envelope.nonce,
            send_manifest: &envelope.send_manifest,
            manifest: &envelope.manifest,
            assertion: &envelope.assertion,
            pow: &envelope.pow,
            serial: *serial,
            blobs: &envelope.blobs,
        };
        if encode_envelope(&parts).as_slice() == body {
            matched = Some(*name);
            break;
        }
    }
    match matched {
        Some(None) => findings.note("  serial: absent"),
        Some(Some(name)) => {
            findings.note(format!("  serial: matches the EXIF serial of {name}"));
        }
        None => findings.fail("the serial matches no input file's EXIF serial"),
    }
}

fn reproduce(
    groups: &[Group],
    seeds: &SeedFile,
    module: &ModuleHost,
    findings: &mut Findings,
) -> Vec<ReproducedImage> {
    let version = seeds.prior_transform_version().get();
    let mut images = Vec::new();
    for group in groups {
        findings.note("");
        findings.note(group.raw.name.clone());
        let Some(key) = LocalImageKey::new(&group.raw.name) else {
            findings.fail(format!("{} is not a usable seed-file key", group.raw.name));
            continue;
        };
        let Some(seed) = seeds.seed_for(&key) else {
            findings.fail(format!("the seed file has no entry for {}", group.raw.name));
            continue;
        };
        let jpegs: Vec<Vec<u8>> = group.jpegs.iter().map(|file| file.bytes.clone()).collect();
        if jpegs.is_empty() {
            findings.note("  no render to prepare");
            continue;
        }
        let request = PrepareRequest {
            raw: group.raw.bytes.clone(),
            jpegs,
            prior: PriorTransformSpec::CameraMatrices { version },
            seed: *seed.as_bytes(),
            params: PrepParams::default(),
        };
        match module.prepare(&request) {
            Ok(response) => {
                findings.note(format!("  prepared {} image(s)", response.images.len()));
                for image in response.images {
                    images.push(ReproducedImage {
                        blob_sha256: sha256(&image.blob),
                        metadata: image.metadata,
                        rerender_group: image.rerender_group,
                        blob: image.blob,
                    });
                }
            }
            Err(error) => findings.fail(undecodable(&group.raw.name, &error)),
        }
    }
    images
}

fn undecodable(name: &str, failure: &ModuleFailure) -> String {
    match failure {
        ModuleFailure::Trapped => format!("{name}: undecodable (the prep module trapped)"),
        other => format!("{name}: the prep module failed: {other}"),
    }
}

fn record_credential(
    state: &mut Option<VerifyState>,
    credentials: &[(String, Vec<u8>)],
    state_path: &Path,
    origin: &Origin,
    rp_id: &str,
    findings: &mut Findings,
) -> Result<()> {
    let Some((_, first)) = credentials.first() else {
        return Ok(());
    };
    let id = hex(first);
    for (label, credential) in &credentials[1..] {
        if credential != first {
            findings.fail(format!(
                "the credential id differs between uploads: {label} carries {}",
                hex(credential)
            ));
            return Ok(());
        }
    }
    let existing = state.as_ref().and_then(|state| {
        state
            .credentials
            .iter()
            .find(|record| record.origin == origin.0 && record.rp_id == rp_id)
    });
    let record = existing.is_none();
    findings.note("");
    match existing {
        None => findings.note(format!(
            "credential id: {id} recorded in {} on first use",
            state_path.display()
        )),
        Some(existing) if existing.credential_id == id => findings.note(format!(
            "credential id: {id} matches the value {} recorded earlier",
            state_path.display()
        )),
        Some(existing) => {
            findings.fail(format!(
                "the credential id {id} differs from {}, which {} records for this site",
                existing.credential_id,
                state_path.display()
            ));
            return Ok(());
        }
    }
    if record && findings.status() != Status::Fail {
        let state = state.get_or_insert_with(VerifyState::default);
        state.credentials.push(CredentialRecord {
            rp_id: rp_id.to_owned(),
            origin: origin.0.clone(),
            credential_id: id,
        });
        state
            .credentials
            .sort_by(|a, b| (&a.rp_id, &a.origin).cmp(&(&b.rp_id, &b.origin)));
        let json = serde_json::to_string_pretty(state).context("serialising the state file")?;
        crate::write(state_path, json.as_bytes())?;
    }
    Ok(())
}

fn pow_params(args: &VerifyPayloadArgs) -> PowParams {
    let defaults = PowParams::default();
    PowParams {
        memory_kib: args.pow_memory_kib.unwrap_or(defaults.memory_kib),
        iterations: args.pow_iterations.unwrap_or(defaults.iterations),
        difficulty_bits_per_image: args
            .pow_difficulty
            .unwrap_or(defaults.difficulty_bits_per_image),
    }
}

fn state_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(dir).join("looklab/verify.json"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".local/state/looklab/verify.json"));
    }
    bail!("set --state, or XDG_STATE_HOME or HOME so the credential id has somewhere to live")
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct VerifyState {
    format: String,
    #[serde(default)]
    credentials: Vec<CredentialRecord>,
}

impl Default for VerifyState {
    fn default() -> Self {
        Self {
            format: STATE_FORMAT.to_owned(),
            credentials: Vec::new(),
        }
    }
}

impl VerifyState {
    fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let state: Self =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if state.format != STATE_FORMAT {
            bail!("{} is not a verify-payload state file", path.display());
        }
        Ok(Some(state))
    }
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct CredentialRecord {
    rp_id: String,
    origin: String,
    credential_id: String,
}

struct ModuleHost {
    engine: Engine,
    module: Module,
}

#[derive(Debug, thiserror::Error)]
enum ModuleFailure {
    #[error("the prep module trapped")]
    Trapped,
    #[error("{0}")]
    Wasmtime(wasmtime::Error),
    #[error("the module reported: {0}")]
    Reported(ModuleError),
    #[error("{0}")]
    Protocol(String),
}

fn wasm_failure(error: wasmtime::Error) -> ModuleFailure {
    if error.downcast_ref::<wasmtime::Trap>().is_some() {
        ModuleFailure::Trapped
    } else {
        ModuleFailure::Wasmtime(error)
    }
}

impl ModuleHost {
    fn open(path: &Path) -> Result<Self> {
        let engine = Engine::default();
        let module = Module::from_file(&engine, path)
            .map_err(|error| anyhow!("loading the prep module {}: {error}", path.display()))?;
        Ok(Self { engine, module })
    }

    fn version(&self) -> Result<VersionResponse, ModuleFailure> {
        match self.invoke(Function::Version, &[])? {
            Response::Version(response) => Ok(response),
            other => Err(ModuleFailure::Protocol(format!(
                "the module answered a version request with {other:?}"
            ))),
        }
    }

    fn prepare(&self, request: &PrepareRequest) -> Result<PrepareResponse, ModuleFailure> {
        let input = postcard::to_allocvec(request)
            .map_err(|error| ModuleFailure::Protocol(error.to_string()))?;
        match self.invoke(Function::Prepare, &input)? {
            Response::Prepare(response) => Ok(response),
            other => Err(ModuleFailure::Protocol(format!(
                "the module answered a prepare request with {other:?}"
            ))),
        }
    }

    fn build_manifest(
        &self,
        request: &BuildManifestRequest,
    ) -> Result<BuildManifestResponse, ModuleFailure> {
        let input = postcard::to_allocvec(request)
            .map_err(|error| ModuleFailure::Protocol(error.to_string()))?;
        match self.invoke(Function::BuildManifest, &input)? {
            Response::BuildManifest(response) => Ok(response),
            other => Err(ModuleFailure::Protocol(format!(
                "the module answered a manifest request with {other:?}"
            ))),
        }
    }

    /// A fresh instance per file (docs/06 §5).
    fn invoke(&self, function: Function, input: &[u8]) -> Result<Response, ModuleFailure> {
        let mut store = Store::new(&self.engine, ());
        let instance = Instance::new(&mut store, &self.module, &[]).map_err(wasm_failure)?;
        let memory = instance.get_memory(&mut store, "memory").ok_or_else(|| {
            ModuleFailure::Protocol("the prep module exports no memory".to_owned())
        })?;
        let alloc = instance
            .get_typed_func::<u32, u32>(&mut store, "ll_alloc")
            .map_err(wasm_failure)?;
        let free = instance
            .get_typed_func::<(u32, u32), ()>(&mut store, "ll_free")
            .map_err(wasm_failure)?;
        let call = instance
            .get_typed_func::<(u32, u32, u32), u64>(&mut store, "ll_call")
            .map_err(wasm_failure)?;

        let len = u32::try_from(input.len())
            .map_err(|_| ModuleFailure::Protocol("the request exceeds 4 GiB".to_owned()))?;
        let in_ptr = alloc.call(&mut store, len).map_err(wasm_failure)?;
        memory
            .write(&mut store, in_ptr as usize, input)
            .map_err(|error| ModuleFailure::Protocol(error.to_string()))?;
        let packed = call
            .call(&mut store, (function.id(), in_ptr, len))
            .map_err(wasm_failure)?;
        free.call(&mut store, (in_ptr, len)).map_err(wasm_failure)?;
        let out_ptr = (packed >> 32) as u32;
        let out_len = (packed & u64::from(u32::MAX)) as u32;
        let mut output = vec![0u8; out_len as usize];
        memory
            .read(&store, out_ptr as usize, &mut output)
            .map_err(|error| ModuleFailure::Protocol(error.to_string()))?;
        free.call(&mut store, (out_ptr, out_len))
            .map_err(wasm_failure)?;

        let (result, rest) = postcard::take_from_bytes::<Result<Response, ModuleError>>(&output)
            .map_err(|error| ModuleFailure::Protocol(error.to_string()))?;
        if !rest.is_empty() {
            return Err(ModuleFailure::Protocol(format!(
                "the module's response has {} trailing bytes",
                rest.len()
            )));
        }
        result.map_err(ModuleFailure::Reported)
    }
}

struct Group {
    raw: InputFile,
    jpegs: Vec<InputFile>,
}

impl Group {
    fn files(&self) -> impl Iterator<Item = &InputFile> {
        std::iter::once(&self.raw).chain(&self.jpegs)
    }
}

struct InputFile {
    name: String,
    bytes: Vec<u8>,
    parsed: Option<ParsedMetadata>,
}

impl InputFile {
    fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let parsed = looklab_meta::parse(&bytes).ok();
        Ok(Self {
            name: file_name(path),
            bytes,
            parsed,
        })
    }

    fn serial(&self) -> Option<&RawSerial> {
        self.parsed.as_ref().and_then(ParsedMetadata::body_serial)
    }
}

fn load_groups(files: &[PathBuf]) -> Result<Vec<Group>> {
    let mut groups: Vec<Group> = Vec::new();
    for path in files {
        match file_kind(path)? {
            FileKind::Raw => groups.push(Group {
                raw: InputFile::load(path)?,
                jpegs: Vec::new(),
            }),
            FileKind::Jpeg => {
                let Some(group) = groups.last_mut() else {
                    bail!(
                        "{} is a JPEG before any RAW; give each group's RAW first",
                        path.display()
                    );
                };
                group.jpegs.push(InputFile::load(path)?);
            }
        }
    }
    if groups.is_empty() {
        bail!("no RAW file was given");
    }
    for group in &mut groups {
        if group.jpegs.is_empty() {
            bail!("{} has no JPEG in its group", group.raw.name);
        }
        // The site pairs a RAW's JPEGs by file name; prep samples the first as the reference.
        group
            .jpegs
            .sort_by(|left, right| left.name.trim().cmp(right.name.trim()));
    }
    Ok(groups)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Raw,
    Jpeg,
}

fn file_kind(path: &Path) -> Result<FileKind> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if RawFormat::from_extension(extension).is_some() {
        return Ok(FileKind::Raw);
    }
    if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
        return Ok(FileKind::Jpeg);
    }
    bail!("{} is not a RAW or JPEG file name", path.display());
}

#[derive(Debug, Deserialize)]
struct Har {
    log: HarLog,
}

#[derive(Debug, Deserialize)]
struct HarLog {
    entries: Vec<HarEntry>,
}

#[derive(Debug, Deserialize)]
struct HarEntry {
    request: HarRequest,
    response: Option<HarResponse>,
}

#[derive(Debug, Deserialize)]
struct HarResponse {
    content: HarResponseContent,
}

#[derive(Debug, Deserialize)]
struct HarResponseContent {
    text: Option<String>,
    encoding: Option<String>,
}

#[derive(Deserialize)]
struct UploadStatus {
    status_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HarRequest {
    method: String,
    url: String,
    #[serde(default)]
    headers: Vec<HarHeader>,
    #[serde(default)]
    post_data: Option<HarPostData>,
}

#[derive(Debug, Deserialize)]
struct HarHeader {
    name: String,
    value: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HarPostData {
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    encoding: Option<String>,
}

impl Har {
    fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }
}

impl HarRequest {
    /// The bytes sent: base64 when the exporter says so, otherwise the text itself.
    fn body(&self) -> Result<Vec<u8>> {
        let Some(post) = self.post_data.as_ref() else {
            return Ok(Vec::new());
        };
        let Some(text) = post.text.as_deref() else {
            return Ok(Vec::new());
        };
        match post.encoding.as_deref() {
            Some("base64") => decode_base64(text),
            Some(other) => bail!("the recorded body uses the unsupported HAR encoding {other:?}"),
            None => Ok(text.as_bytes().to_vec()),
        }
    }
}

fn decode_base64(text: &str) -> Result<Vec<u8>> {
    let text = text.trim();
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(text))
        .context("the recorded body is not base64")
}

/// A request header outside this list can carry data off the site.
fn is_browser_header(name: &str) -> bool {
    const ALLOWED: &[&str] = &[
        "accept",
        "accept-encoding",
        "accept-language",
        "cache-control",
        "connection",
        "content-length",
        "content-type",
        "cookie",
        "dnt",
        "expect",
        "host",
        "if-match",
        "if-modified-since",
        "if-none-match",
        "if-range",
        "if-unmodified-since",
        "origin",
        "pragma",
        "priority",
        "range",
        "referer",
        "te",
        "upgrade-insecure-requests",
        "user-agent",
    ];
    let name = name.to_ascii_lowercase();
    name.starts_with("sec-") || ALLOWED.contains(&name.as_str())
}

fn audit_request(
    request: &HarRequest,
    class: RequestClass,
    service: &Service<'_>,
    findings: &mut Findings,
    credentials: &mut Vec<(String, Vec<u8>)>,
) {
    for header in &request.headers {
        if class == RequestClass::SessionStatus && header.name.eq_ignore_ascii_case("authorization")
        {
            continue;
        }
        if !is_browser_header(&header.name) {
            findings.review(format!(
                "request {} {} carries header {:?}: {:?}",
                request.method, request.url, header.name, header.value
            ));
        }
    }
    match class {
        RequestClass::Nonce | RequestClass::Status | RequestClass::SessionStatus => {
            match request.body() {
                Ok(body) if body.is_empty() => {}
                Ok(body) => findings.fail(format!(
                    "request {} {} carries a body, but this endpoint takes none: {:?}",
                    request.method,
                    request.url,
                    String::from_utf8_lossy(&body)
                )),
                Err(error) => findings.fail(format!(
                    "request {} {} body: {error}",
                    request.method, request.url
                )),
            }
        }
        RequestClass::Session => audit_session(request, service, findings),
        RequestClass::RegisterStart => {
            audit_registration(request, &["display_handle"], findings);
        }
        RequestClass::RegisterFinish => {
            audit_registration(request, &["handle", "credential"], findings);
        }
        RequestClass::ManualWithdrawal => audit_withdrawal(request, service, findings, credentials),
        RequestClass::ManualSummary => audit_manual_summary(request, service, findings),
        RequestClass::Upload | RequestClass::ManualConfirmation | RequestClass::Unexpected => {}
    }
}

fn audit_manual_summary(request: &HarRequest, service: &Service<'_>, findings: &mut Findings) {
    if request.post_data.is_some() {
        findings.fail(format!(
            "request {} {} carries a body, but the camera summary is a GET",
            request.method, request.url
        ));
        return;
    }
    let Some(parts) = UrlParts::parse(&request.url) else {
        return;
    };
    let Some(camera) = manual_summary_camera(parts.path()) else {
        return;
    };
    let Ok(camera_id) = BodyId::new(camera) else {
        findings.fail(format!(
            "the camera summary names an invalid camera {:?}",
            escaped(camera)
        ));
        return;
    };
    let Some(manuals) = service.manuals else {
        findings.fail(
            "the camera summary has no local documented manual catalog to resolve its camera",
        );
        return;
    };
    let Some(catalog_camera) = manuals.camera(&camera_id) else {
        findings.fail(format!(
            "the camera summary names {}, which is not in the local documented manual catalog",
            escaped(camera)
        ));
        return;
    };
    findings.note(format!(
        "  camera summary target: {} ({} {})",
        escaped(catalog_camera.id.as_str()),
        escaped(&catalog_camera.make),
        escaped(&catalog_camera.model)
    ));
}

fn audit_session_status(request: &HarRequest, har: &Har, origin: &Origin, findings: &mut Findings) {
    let mut headers = request
        .headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("authorization"));
    let token = headers
        .next()
        .and_then(|header| header.value.strip_prefix("Bearer "))
        .and_then(StatusToken::from_base64url);
    let Some(token) = token else {
        findings.fail("session restoration requires Authorization: Bearer with a 32-byte base64url status token");
        return;
    };
    if headers.next().is_some() {
        findings.fail("session restoration carries multiple Authorization headers");
    }
    for entry in &har.log.entries {
        if classify(&entry.request, origin) != RequestClass::Upload {
            continue;
        }
        let Some(content) = entry.response.as_ref().map(|response| &response.content) else {
            continue;
        };
        let Some(text) = &content.text else {
            continue;
        };
        let bytes = if content.encoding.as_deref() == Some("base64") {
            decode_base64(text)
        } else {
            Ok(text.as_bytes().to_vec())
        };
        let uploaded = bytes
            .ok()
            .and_then(|bytes| serde_json::from_slice::<UploadStatus>(&bytes).ok());
        if let Some(uploaded) = uploaded
            && StatusToken::from_base64url(&uploaded.status_token).as_ref() != Some(&token)
        {
            findings.fail("session restoration status token differs from the upload response");
        }
    }
}

const SESSION_FIELDS: &[&str] = &[
    "nonce",
    "credential_id",
    "authenticator_data",
    "client_data_json",
    "signature",
    "pow",
];

fn audit_session(request: &HarRequest, service: &Service<'_>, findings: &mut Findings) {
    let body = match request.body() {
        Ok(body) => body,
        Err(error) => {
            findings.fail(format!(
                "request {} {} body: {error}",
                request.method, request.url
            ));
            return;
        }
    };
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            findings.fail(format!(
                "request {} {} body is not JSON: {error}",
                request.method, request.url
            ));
            return;
        }
    };
    let Some(object) = value.as_object() else {
        findings.fail(format!(
            "request {} {} body is not a JSON object",
            request.method, request.url
        ));
        return;
    };
    for key in object.keys() {
        if !SESSION_FIELDS.contains(&key.as_str()) {
            findings.fail(format!(
                "request {} {} body carries an unaccounted key {key:?}",
                request.method, request.url
            ));
        }
    }
    let field = |name: &str| object.get(name).and_then(serde_json::Value::as_str);
    let mut decoded = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    for (slot, name) in decoded.iter_mut().zip([
        "credential_id",
        "authenticator_data",
        "client_data_json",
        "signature",
    ]) {
        let Some(text) = field(name) else {
            findings.fail(format!(
                "request {} {} body has no {name}",
                request.method, request.url
            ));
            return;
        };
        match decode_url_field(text) {
            Ok(bytes) => *slot = bytes,
            Err(error) => {
                findings.fail(format!(
                    "request {} {} body's {name} is not base64url: {error}",
                    request.method, request.url
                ));
                return;
            }
        }
    }
    if let Some(pow) = object.get("pow")
        && !pow.as_array().is_some_and(|values| {
            values.len() == looklab_upload::POW_K && values.iter().all(serde_json::Value::is_u64)
        })
    {
        findings.fail(format!(
            "request {} {} body's pow is not {POW_K} unsigned integers",
            request.method,
            request.url,
            POW_K = looklab_upload::POW_K
        ));
    }
    let Some(nonce_text) = field("nonce") else {
        findings.fail(format!(
            "request {} {} body has no nonce",
            request.method, request.url
        ));
        return;
    };
    let nonce = match decode_url_field(nonce_text)
        .ok()
        .and_then(|bytes| Nonce::from_slice(&bytes))
    {
        Some(nonce) => nonce,
        None => {
            findings.fail(format!(
                "request {} {} body's nonce is not a 32-byte base64url value",
                request.method, request.url
            ));
            return;
        }
    };
    let [
        credential_id,
        authenticator_data,
        client_data_json,
        signature,
    ] = decoded;
    let assertion = WebAuthnAssertion {
        credential_id,
        authenticator_data,
        client_data_json,
        signature,
    };
    let challenge = Challenge::new(&nonce, &request_digest(RequestDomain::Session, b""));
    check_assertion(
        &assertion,
        service,
        challenge,
        "H(nonce ‖ H(looklab-request-v1:session:))",
        findings,
    );
}

fn decode_url_field(value: &str) -> std::result::Result<Vec<u8>, base64::DecodeError> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(value)
}

fn audit_registration(request: &HarRequest, allowed: &[&str], findings: &mut Findings) {
    let body = match request.body() {
        Ok(body) => body,
        Err(error) => {
            findings.fail(format!(
                "request {} {} body: {error}",
                request.method, request.url
            ));
            return;
        }
    };
    findings.note(format!(
        "  {} {} body: {}",
        request.method,
        request.url,
        String::from_utf8_lossy(&body)
    ));
    if body.is_empty() {
        findings.fail(format!(
            "request {} {} has no JSON body",
            request.method, request.url
        ));
        return;
    }
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            findings.fail(format!(
                "request {} {} body is not JSON: {error}",
                request.method, request.url
            ));
            return;
        }
    };
    let Some(object) = value.as_object() else {
        findings.fail(format!(
            "request {} {} body is not a JSON object",
            request.method, request.url
        ));
        return;
    };
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            findings.fail(format!(
                "request {} {} body carries an unaccounted key {key:?}",
                request.method, request.url
            ));
        }
    }
    for key in allowed {
        if !object.contains_key(*key) {
            findings.fail(format!("registration body is missing {key:?}"));
        }
    }
    if allowed == ["display_handle"] {
        if !object.get("display_handle").is_some_and(|value| {
            value.is_null()
                || value.as_str().is_some_and(|handle| {
                    !handle.is_empty()
                        && handle.chars().count() <= 64
                        && !handle.chars().any(char::is_control)
                })
        }) {
            findings.fail("registration display_handle must be null or a valid display handle");
        }
        return;
    }
    if !object
        .get("handle")
        .is_some_and(serde_json::Value::is_string)
    {
        findings.fail("registration handle must be a string");
    }
    let Some(credential) = object.get("credential") else {
        return;
    };
    audit_registration_credential(credential, findings);
}

fn audit_registration_credential(credential: &serde_json::Value, findings: &mut Findings) {
    let Some(object) = credential.as_object() else {
        findings.fail("registration credential must be an object");
        return;
    };
    for key in object.keys() {
        if ![
            "id",
            "rawId",
            "type",
            "response",
            "authenticatorAttachment",
            "clientExtensionResults",
        ]
        .contains(&key.as_str())
        {
            findings.fail(format!(
                "registration credential carries an unaccounted key {key:?}"
            ));
        }
    }
    for key in ["id", "rawId"] {
        if !object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| {
                decode_url_field(text).is_ok_and(|bytes| !bytes.is_empty() && bytes.len() <= 255)
            })
        {
            findings.fail(format!(
                "registration credential {key} must be a base64url credential ID"
            ));
        }
    }
    if object.get("id") != object.get("rawId") {
        findings.fail("registration credential id and rawId differ");
    }
    if object.get("type").and_then(serde_json::Value::as_str) != Some("public-key") {
        findings.fail("registration credential type must be public-key");
    }
    if let Some(attachment) = object.get("authenticatorAttachment")
        && !attachment.is_null()
        && !matches!(attachment.as_str(), Some("platform" | "cross-platform"))
    {
        findings.fail("registration authenticatorAttachment is not a WebAuthn attachment");
    }
    match object
        .get("clientExtensionResults")
        .and_then(serde_json::Value::as_object)
    {
        Some(extensions) if extensions.is_empty() => {}
        Some(extensions) => {
            findings.review(format!("registration client extensions: {extensions:?}"))
        }
        None => findings.fail("registration clientExtensionResults must be an object"),
    }
    let Some(response) = object
        .get("response")
        .and_then(serde_json::Value::as_object)
    else {
        findings.fail("registration credential response must be an object");
        return;
    };
    for key in response.keys() {
        if ![
            "clientDataJSON",
            "attestationObject",
            "authenticatorData",
            "publicKey",
            "publicKeyAlgorithm",
            "transports",
        ]
        .contains(&key.as_str())
        {
            findings.fail(format!(
                "registration response carries an unaccounted key {key:?}"
            ));
        }
    }
    for key in [
        "clientDataJSON",
        "attestationObject",
        "authenticatorData",
        "publicKey",
    ] {
        match response.get(key) {
            None if matches!(key, "authenticatorData" | "publicKey") => {}
            Some(value) if key == "publicKey" && value.is_null() => {}
            Some(value)
                if value
                    .as_str()
                    .is_some_and(|text| decode_url_field(text).is_ok()) => {}
            _ => findings.fail(format!("registration response {key} must be base64url")),
        }
    }
    if let Some(algorithm) = response.get("publicKeyAlgorithm")
        && !algorithm.is_i64()
    {
        findings.fail("registration publicKeyAlgorithm must be an integer");
    }
    if let Some(transports) = response.get("transports")
        && !transports.as_array().is_some_and(|values| {
            values.iter().all(|value| {
                matches!(
                    value.as_str(),
                    Some("usb" | "nfc" | "ble" | "internal" | "hybrid" | "smart-card")
                )
            })
        })
    {
        findings.fail("registration transports must contain WebAuthn transports");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestClass {
    Nonce,
    Upload,
    ManualConfirmation,
    ManualWithdrawal,
    ManualSummary,
    Status,
    Session,
    SessionStatus,
    RegisterStart,
    RegisterFinish,
    Unexpected,
}

fn classify(request: &HarRequest, origin: &Origin) -> RequestClass {
    let Some(parts) = UrlParts::parse(&request.url) else {
        return RequestClass::Unexpected;
    };
    if parts.origin() != origin.0 {
        return RequestClass::Unexpected;
    }
    if parts.path_and_query != parts.path() {
        return RequestClass::Unexpected;
    }
    match (request.method.as_str(), parts.path()) {
        ("POST", NONCE_PATH) => RequestClass::Nonce,
        ("POST", UPLOAD_PATH) => RequestClass::Upload,
        ("POST", MANUAL_CONFIRMATION_PATH) => RequestClass::ManualConfirmation,
        ("POST", MANUAL_WITHDRAWAL_PATH) => RequestClass::ManualWithdrawal,
        ("POST", SESSION_PATH) => RequestClass::Session,
        ("GET", SESSION_PATH) => RequestClass::SessionStatus,
        ("POST", REGISTER_START_PATH) => RequestClass::RegisterStart,
        ("POST", REGISTER_FINISH_PATH) => RequestClass::RegisterFinish,
        ("GET", path) if manual_summary_camera(path).is_some() => RequestClass::ManualSummary,
        ("GET", path) if status_token(path).is_some() => RequestClass::Status,
        _ => RequestClass::Unexpected,
    }
}

fn manual_summary_camera(path: &str) -> Option<&str> {
    let camera = path.strip_prefix(MANUAL_SUMMARY_PREFIX)?;
    let (make, model) = camera.split_once('/')?;
    (!make.is_empty() && !model.is_empty() && !model.contains('/')).then_some(camera)
}

fn status_token(path: &str) -> Option<&str> {
    let token = path.strip_prefix(STATUS_PREFIX)?;
    (!token.is_empty() && !token.contains('/')).then_some(token)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Origin(String);

impl Origin {
    fn parse(origin: &str) -> Result<Self> {
        let parts = UrlParts::parse(origin).context("not a URL")?;
        if !parts.path_and_query.is_empty() {
            bail!("an origin has no path; drop the trailing slash or path");
        }
        if parts.scheme != "https" && parts.scheme != "http" {
            bail!("an origin starts with http:// or https://");
        }
        Ok(Self(parts.origin()))
    }
}

impl std::fmt::Display for Origin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UrlParts {
    scheme: String,
    authority: String,
    path_and_query: String,
}

impl UrlParts {
    fn parse(url: &str) -> Option<Self> {
        let (scheme, rest) = url.split_once("://")?;
        if scheme.is_empty() {
            return None;
        }
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..end];
        if authority.is_empty() {
            return None;
        }
        Some(Self {
            scheme: scheme.to_ascii_lowercase(),
            authority: authority.to_ascii_lowercase(),
            path_and_query: rest[end..].to_owned(),
        })
    }

    fn origin(&self) -> String {
        format!("{}://{}", self.scheme, self.authority)
    }

    fn path(&self) -> &str {
        self.path_and_query
            .split(['?', '#'])
            .next()
            .unwrap_or_default()
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut output = [0u8; 32];
    output.copy_from_slice(&digest);
    output
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(DIGITS[(byte >> 4) as usize] as char);
        text.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    text
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}
