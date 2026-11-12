// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end tests of the axum app against an in-process router, a temporary
//! SQLite file and a temporary blob directory.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use looklab_samples::{
    InputSpace, OutputSpace, PatchId, PatchRow, PatchTable, PatchTableHeader, PrepParamsHash,
    PrepVersion, PriorTransformVersion, RerenderGroupId, SampleFlags, SampleHeader, SampleSet,
};
use looklab_schema::{
    CapabilityKey, ConfirmationScope, ControlObservation, Date, Finding, FixtureDonation,
    FixtureLicense, FixtureUpload, LabeledTag, MakernoteValue, ManualCamera, ManualConfirmation,
    ManualUrl, ModeratorEvidence, ModeratorEvidenceKind, ModeratorFirmwareScope, Placement,
    ProposalKind, ReportedFirmware, Sha256Hex, frame_fixture_upload, frame_lut_proposal,
    validate_manual_catalog,
};
use looklab_server::body_schema::Bodies;
use looklab_server::{AppState, Config, PowConfig, router};
use looklab_upload::{
    AttestationHash, BatchManifest, BlobKind, BodyId, CardDeclaration, Challenge,
    CommunitySnapshotVersion, ContributionKind, ControlEntry, ControlKey, ControlSettings,
    EnvelopeParts, FirmwareVersionString, Intent, Limits, ManifestHash, ManifestImage,
    ManifestMetadata, ManualConfirmationRequest, ManualModeration, ManualWithdrawalRequest,
    MonochromeAuthorization, Nonce, PowParams, PowSolutions, RawSerial, RenderClass,
    SEND_MANIFEST_VERSION, SendManifest, SerialHmacKey, SettingSource, SimKey, StepIndex,
    WebAuthnAssertion, canonical_send_manifest_bytes, encode_envelope, manifest_hash,
    parse_render_tag, send_manifest_hash, solve,
};
use p256::ecdsa::{Signature, SigningKey, signature::Signer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Row, SqlitePool};
use tempfile::TempDir;
use tower::ServiceExt as _;
use webauthn_authenticator_rs::AuthenticatorBackendHashedClientData;
use webauthn_authenticator_rs::prelude::{CreationChallengeResponse, RequestChallengeResponse};
use webauthn_authenticator_rs::prelude::{Url as AuthUrl, WebauthnAuthenticator};
use webauthn_authenticator_rs::softpasskey::SoftPasskey;

const ORIGIN: &str = "https://looklab.example";
const RP_ID: &str = "looklab.example";
const SERIAL: &str = "SN1234567890";

fn pow_params() -> PowParams {
    PowParams {
        memory_kib: 8,
        iterations: 1,
        difficulty_bits_per_image: 5.0,
    }
}

struct BufferWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for BufferWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

static LOGS: LazyLock<Arc<Mutex<Vec<u8>>>> = LazyLock::new(|| {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || BufferWriter(writer.clone()))
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
    buffer
});

fn logs() -> &'static Arc<Mutex<Vec<u8>>> {
    &LOGS
}

fn log_bytes() -> Vec<u8> {
    logs().lock().expect("log buffer").clone()
}

struct TestServer {
    dir: TempDir,
    app: Router,
    state: Arc<AppState>,
    db: SqlitePool,
    blob_dir: PathBuf,
    soft: SoftPasskey,
    credential_id: Vec<u8>,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with_community(|_| {}).await
    }

    async fn start_with_community(build: impl FnOnce(&Path)) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let bodies = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/bodies");
        Self::start_in(dir, bodies, build).await
    }

    /// Serves a copy of the shipped documents, so a test can add one after a snapshot exists.
    async fn start_with_shipped_copy() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
        let data = dir.path().join("data");
        for sub in ["bodies", "interfaces"] {
            copy_shipped_documents(&shipped.join(sub), &data.join(sub));
        }
        Self::start_in(dir, data.join("bodies"), |_| {}).await
    }

    async fn start_in(dir: TempDir, bodies_dir: PathBuf, build: impl FnOnce(&Path)) -> Self {
        let _ = logs();
        let secret = dir.path().join("server-secret");
        std::fs::write(&secret, [9u8; 32]).expect("write secret");
        let community = dir.path().join("community");
        build(&community);
        let config = Config {
            bind: "127.0.0.1:0".parse().expect("bind"),
            db_path: dir.path().join("looklab.sqlite3"),
            blob_dir: dir.path().join("blobs"),
            rp_id: RP_ID.to_owned(),
            origin: ORIGIN.to_owned(),
            server_secret_path: secret,
            bodies_dir: bodies_dir.clone(),
            manuals_dir: Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/manuals"),
            snapshot_dir: community,
            nonce_ttl: Duration::from_secs(300),
            registration_ttl: Duration::from_secs(300),
            body_timeout: Duration::from_secs(30),
            blob_retention_days: 30,
            cors_origins: Vec::new(),
            site: Default::default(),
            pow: PowConfig {
                memory_kib: 8,
                iterations: 1,
                difficulty_bits_per_image: 5.0,
            },
            limits: Limits {
                max_body_bytes: 1 << 20,
                max_manifest_bytes: 1 << 16,
                max_images: 8,
                max_blob_bytes: 1 << 16,
                max_batches: 8,
                max_attestations: 8,
            },
        };
        let db_path = config.db_path.clone();
        let blob_dir = config.blob_dir.clone();
        let state = Arc::new(AppState::new(config).await.expect("state"));
        if let Some(version) = highest_snapshot(&state.config.snapshot_dir) {
            sqlx::query(
                "INSERT INTO community_snapshot_activation
                 (id, version, content_hash, activated_at)
                 VALUES (1, ?, ?, 0)",
            )
            .bind(i64::try_from(version).expect("snapshot version"))
            .bind("0".repeat(64))
            .execute(&state.db)
            .await
            .expect("fixture activation");
            *state.bodies.write().await =
                Bodies::load_active(&bodies_dir, &state.config.snapshot_dir, Some(version))
                    .expect("fixture bodies");
        }
        let app = router(state.clone());
        let db = SqlitePool::connect_with(SqliteConnectOptions::new().filename(&db_path))
            .await
            .expect("test db");
        let mut soft = SoftPasskey::new(true);
        let credential_id = register(&app, &mut soft).await;
        Self {
            dir,
            app,
            state,
            db,
            blob_dir,
            soft,
            credential_id,
        }
    }

    async fn nonce(&self) -> Nonce {
        let request = Request::builder()
            .method("POST")
            .uri("/api/nonce")
            .body(Body::empty())
            .expect("request");
        let (status, _, body) = send(&self.app, request).await;
        assert_eq!(status, StatusCode::CREATED, "nonce: {body:?}");
        let value: Value = serde_json::from_slice(&body).expect("nonce json");
        let bytes = URL_SAFE_NO_PAD
            .decode(value["nonce"].as_str().expect("nonce string"))
            .expect("nonce base64");
        Nonce::from_slice(&bytes).expect("nonce length")
    }

    async fn assertion(&mut self, challenge: &Challenge) -> WebAuthnAssertion {
        self.assertion_with_verification(challenge, true).await
    }

    async fn assertion_with_verification(
        &mut self,
        challenge: &Challenge,
        required: bool,
    ) -> WebAuthnAssertion {
        let options =
            request_options(challenge.as_bytes(), &self.credential_id, required).public_key;
        let client_data_json = format!(
            r#"{{"type":"webauthn.get","challenge":"{}","origin":"{ORIGIN}"}}"#,
            URL_SAFE_NO_PAD.encode(challenge.as_bytes())
        );
        let hash = Sha256::digest(client_data_json.as_bytes()).to_vec();
        let credential = self
            .soft
            .perform_auth(hash, options, 60_000)
            .expect("software authenticator");
        WebAuthnAssertion {
            credential_id: credential.raw_id,
            authenticator_data: credential.response.authenticator_data,
            client_data_json: client_data_json.into_bytes(),
            signature: credential.response.signature,
        }
    }

    async fn credential_uuid(&self) -> String {
        self.credential_uuid_for(&self.credential_id).await
    }

    async fn credential_uuid_for(&self, raw_id: &[u8]) -> String {
        sqlx::query_scalar("SELECT id FROM credentials WHERE credential_id = ?")
            .bind(raw_id)
            .fetch_one(&self.db)
            .await
            .expect("credential id")
    }

    async fn persist_credential(&self) -> String {
        let (passkey, display_handle) = self
            .state
            .pending_credential(&self.credential_id)
            .expect("pending credential");
        let id = looklab_server::CredentialId::new();
        let passkey = serde_json::to_string(&passkey).expect("passkey json");
        sqlx::query(
            "INSERT INTO credentials
             (id, credential_id, webauthn_credential, display_handle, tier, roles, reputation,
              created_at, status)
             VALUES (?, ?, ?, ?, 'new', '[]', 0.5, 0, 'active')",
        )
        .bind(id.as_str())
        .bind(&self.credential_id)
        .bind(passkey)
        .bind(display_handle)
        .execute(&self.db)
        .await
        .expect("credential");
        id.as_str().to_owned()
    }

    async fn add_credential(&self) -> String {
        let id = looklab_server::CredentialId::new();
        sqlx::query(
            "INSERT INTO credentials (id, credential_id, webauthn_credential, created_at)
             VALUES (?, ?, '{}', 0)",
        )
        .bind(id.as_str())
        .bind(id.as_str().as_bytes())
        .execute(&self.db)
        .await
        .expect("credential");
        id.as_str().to_owned()
    }

    async fn set_credential_tier_roles(&self, raw_id: &[u8], tier: &str, roles: &str) {
        sqlx::query("UPDATE credentials SET tier = ?, roles = ? WHERE credential_id = ?")
            .bind(tier)
            .bind(roles)
            .bind(raw_id)
            .execute(&self.db)
            .await
            .expect("update credential");
    }

    async fn signed_fields(&mut self, domain: &str, payload: &[u8]) -> Value {
        let nonce = self.nonce().await;
        let digest = action_digest(domain, payload);
        let challenge = Challenge::new(&nonce, &digest);
        let assertion = self.assertion(&challenge).await;
        json!({
            "nonce": b64(nonce.as_bytes()),
            "credential_id": b64(&assertion.credential_id),
            "authenticator_data": b64(&assertion.authenticator_data),
            "client_data_json": b64(&assertion.client_data_json),
            "signature": b64(&assertion.signature),
        })
    }

    async fn withdrawal_request(&mut self, camera: &BodyId) -> ManualWithdrawalRequest {
        let mut signed = self
            .signed_fields("withdraw-manual-confirmation", camera.as_str().as_bytes())
            .await;
        signed["camera"] = json!(camera.as_str());
        serde_json::from_value(signed).expect("withdrawal request")
    }

    async fn post_signed(
        &mut self,
        path: &str,
        domain: &str,
        payload: &[u8],
        mut body: Value,
    ) -> (StatusCode, Value) {
        let signed = self.signed_fields(domain, payload).await;
        for (key, value) in signed.as_object().expect("signed object") {
            body[key.as_str()] = value.clone();
        }
        post_json(&self.app, path, &body).await
    }

    async fn upload_bytes(&self, bytes: Vec<u8>) -> (StatusCode, Value) {
        upload_request(&self.app, bytes).await
    }

    async fn upload_valid(&mut self, serial: Option<&RawSerial>) -> (StatusCode, Value) {
        let (manifest, blobs) = manifest(valid_settings());
        self.upload_manifest(&manifest, &blobs, serial).await
    }

    async fn upload_manifest(
        &mut self,
        manifest: &BatchManifest,
        blobs: &[Vec<u8>],
        serial: Option<&RawSerial>,
    ) -> (StatusCode, Value) {
        let nonce = self.nonce().await;
        let pow_challenge = Challenge::new(&nonce, &manifest_hash(manifest));
        let challenge = send_challenge(&nonce, manifest);
        let assertion = self.assertion(&challenge).await;
        let pow = solve(
            &pow_challenge,
            u32::try_from(manifest.images.len()).expect("image count"),
            &pow_params(),
            0,
        )
        .expect("proof-of-work");
        let bytes = envelope(nonce, manifest, blobs, serial, &assertion, &pow);
        self.upload_bytes(bytes).await
    }

    async fn get(&self, path: &str) -> (StatusCode, HeaderMap, Bytes) {
        let request = Request::builder()
            .method("GET")
            .uri(path)
            .body(Body::empty())
            .expect("request");
        send(&self.app, request).await
    }

    async fn listen(&self) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("listener address");
        let app = router(self.state.clone());
        tokio::spawn(async move {
            let service = app.into_make_service_with_connect_info::<SocketAddr>();
            let _ = axum::serve(listener, service).await;
        });
        addr
    }
}

#[tokio::test]
async fn nonce_advertises_the_configured_parent_rp_id() {
    let dir = tempfile::tempdir().expect("temp dir");
    let secret = dir.path().join("server-secret");
    std::fs::write(&secret, [3u8; 32]).expect("write secret");
    let config = Config {
        bind: "127.0.0.1:0".parse().expect("bind"),
        db_path: dir.path().join("looklab.sqlite3"),
        blob_dir: dir.path().join("blobs"),
        rp_id: "looklab.example".to_owned(),
        origin: "https://photos.looklab.example".to_owned(),
        server_secret_path: secret,
        bodies_dir: Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/bodies"),
        manuals_dir: Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/manuals"),
        snapshot_dir: dir.path().join("community"),
        nonce_ttl: Duration::from_secs(300),
        registration_ttl: Duration::from_secs(300),
        body_timeout: Duration::from_secs(30),
        blob_retention_days: 30,
        cors_origins: Vec::new(),
        site: Default::default(),
        pow: PowConfig {
            memory_kib: 8,
            iterations: 1,
            difficulty_bits_per_image: 5.0,
        },
        limits: Limits {
            max_body_bytes: 1 << 20,
            max_manifest_bytes: 1 << 16,
            max_images: 8,
            max_blob_bytes: 1 << 16,
            max_batches: 8,
            max_attestations: 8,
        },
    };
    let state = Arc::new(AppState::new(config).await.expect("state"));
    let app = router(state);
    let request = Request::builder()
        .method("POST")
        .uri("/api/nonce")
        .body(Body::empty())
        .expect("request");
    let (status, _, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::CREATED, "nonce: {body:?}");
    let value: Value = serde_json::from_slice(&body).expect("nonce json");
    assert_eq!(value["rp_id"], "looklab.example");
}

async fn tcp_nonce(addr: SocketAddr) -> (u16, Vec<u8>) {
    tcp_request(addr, "POST", "/api/nonce", None, &[]).await
}

async fn tcp_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    content_type: Option<&str>,
    body: &[u8],
) -> (u16, Vec<u8>) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
         X-Forwarded-For: {SENTINEL}\r\nX-Real-IP: {SENTINEL}\r\n\
         Forwarded: for={SENTINEL}\r\nCF-Connecting-IP: {SENTINEL}\r\n"
    );
    if let Some(content_type) = content_type {
        request.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    let mut bytes = request.into_bytes();
    bytes.extend_from_slice(body);
    stream.write_all(&bytes).await.expect("write request");
    stream.flush().await.expect("flush request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read response");
    parse_response(&response)
}

fn parse_response(response: &[u8]) -> (u16, Vec<u8>) {
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response headers");
    let head = std::str::from_utf8(&response[..split]).expect("response head");
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("status code");
    (status, response[split + 4..].to_vec())
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("in-process request");
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), 16 << 20)
        .await
        .expect("response body");
    (status, headers, body)
}

async fn upload_request(app: &Router, bytes: Vec<u8>) -> (StatusCode, Value) {
    let length = bytes.len();
    let request = Request::builder()
        .method("POST")
        .uri("/api/upload")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, length)
        .body(Body::from(bytes))
        .expect("request");
    let (status, _, body) = send(app, request).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn post_json(app: &Router, path: &str, value: &Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(value).expect("json")))
        .expect("request");
    let (status, _, body) = send(app, request).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn post_confirmation(
    app: &Router,
    request: &ManualConfirmationRequest,
) -> (StatusCode, Value) {
    post_canonical(app, "/api/manual-confirmations", request.canonical_bytes()).await
}

async fn post_withdrawal(app: &Router, request: &ManualWithdrawalRequest) -> (StatusCode, Value) {
    post_canonical(
        app,
        "/api/manual-confirmations/withdraw",
        request.canonical_bytes(),
    )
    .await
}

async fn post_canonical(app: &Router, path: &str, body: Vec<u8>) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("request");
    let (status, _, body) = send(app, request).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn register(app: &Router, soft: &mut SoftPasskey) -> Vec<u8> {
    let (status, body) = post_json(
        app,
        "/api/passkey/register/start",
        &json!({ "display_handle": "tester" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "register start: {body}");
    let handle = body["handle"].as_str().expect("handle").to_owned();
    let mut options: CreationChallengeResponse =
        serde_json::from_value(body["options"].clone()).expect("creation options");
    // SoftPasskey does not implement resident-key storage.
    options
        .public_key
        .authenticator_selection
        .as_mut()
        .expect("authenticator selection")
        .require_resident_key = false;
    let credential = soft
        .do_registration(AuthUrl::parse(ORIGIN).expect("origin"), options)
        .expect("software authenticator registration");
    let raw_id = credential.raw_id.clone();
    let (status, body) = post_json(
        app,
        "/api/passkey/register/finish",
        &json!({ "handle": handle, "credential": credential }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "register finish: {body}");
    raw_id
}

#[tokio::test(flavor = "multi_thread")]
async fn registration_requires_a_discoverable_user_verified_credential() {
    let server = TestServer::start().await;
    let (status, body) = post_json(&server.app, "/api/passkey/register/start", &json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let selection = &body["options"]["publicKey"]["authenticatorSelection"];
    assert_eq!(selection["residentKey"], "required");
    assert_eq!(selection["requireResidentKey"], true);
    assert_eq!(selection["userVerification"], "required");
}

fn request_options(
    challenge: &[u8],
    credential_id: &[u8],
    verification_required: bool,
) -> RequestChallengeResponse {
    let verification = if verification_required {
        "required"
    } else {
        "discouraged"
    };
    serde_json::from_value(json!({
        "publicKey": {
            "challenge": URL_SAFE_NO_PAD.encode(challenge),
            "rpId": RP_ID,
            "allowCredentials": [{
                "type": "public-key",
                "id": URL_SAFE_NO_PAD.encode(credential_id),
            }],
            "userVerification": verification,
        },
    }))
    .expect("request options")
}

fn metadata() -> ManifestMetadata {
    ManifestMetadata {
        make: Some("Fujifilm".to_owned()),
        model: Some("X-T5".to_owned()),
        firmware: Some("2.10".to_owned()),
        lens_model: Some("XF 23mm F1.4".to_owned()),
        focal_length_mm: Some(23.0),
        iso: Some(400),
        shutter_seconds: Some(0.008),
        aperture_f_number: Some(1.4),
        exposure_compensation_ev: Some(-0.33),
        as_shot_wb: None,
        capture_month: None,
        software: Some("X-T5 Ver.2.10".to_owned()),
        render_tag: None,
        render_tag_malformed: false,
        look: None,
    }
}

fn sample_blob() -> Vec<u8> {
    let header = SampleHeader {
        n: 1,
        input_space: InputSpace::CameraRgbWbLinear,
        output_space: OutputSpace::Srgb,
        wb_multipliers: [1.0, 1.0, 1.0],
        cct_hint_k: 5000.0,
        exposure_hint_ev: 0.0,
        rerender_group: Some(RerenderGroupId::new([7; 16])),
        prep_version: PrepVersion::new(1),
        prior_transform_version: PriorTransformVersion::new(1),
        prep_params_hash: PrepParamsHash::new([0; 32]),
    };
    let set = SampleSet::new(
        header,
        vec![[0.1, 0.2, 0.3]],
        vec![[0.4, 0.5, 0.6]],
        vec![1.0],
        vec![SampleFlags::NONE],
    )
    .expect("sample set");
    looklab_samples::encode(&set).expect("sample blob")
}

fn patch_blob() -> Vec<u8> {
    let header = PatchTableHeader {
        n: 1,
        input_space: InputSpace::CameraRgbWbLinear,
        output_space: OutputSpace::Srgb,
        prep_version: PrepVersion::new(1),
    };
    let row = PatchRow {
        id: PatchId::new(2),
        in_mean: [0.1, 0.2, 0.3],
        in_std: [0.01, 0.02, 0.03],
        out_mean: [0.4, 0.5, 0.6],
        out_std: [0.02, 0.02, 0.02],
    };
    let table = PatchTable::new(header, vec![row]).expect("patch table");
    looklab_samples::encode_patch_table(&table).expect("patch blob")
}

fn image(blob: &[u8], kind: BlobKind, rerender_group: Option<RerenderGroupId>) -> ManifestImage {
    ManifestImage {
        meta: metadata(),
        rerender_group,
        blob_sha256: Sha256::digest(blob).into(),
        blob_kind: kind,
    }
}

fn entry(key: &str, step: i64, source: SettingSource) -> ControlEntry {
    ControlEntry::new(
        ControlKey::new(key).expect("control key"),
        StepIndex::new(step),
        source,
    )
}

fn reproduced(key: &str, step: i64) -> ControlEntry {
    entry(key, step, SettingSource::Reproduced)
}

fn declared(key: &str, step: i64) -> ControlEntry {
    entry(key, step, SettingSource::Declared)
}

fn typed_monochrome_look(raw: i64) -> looklab_upload::LookSettings {
    looklab_upload::LookSettings {
        saturation: Some(looklab_upload::Tagged {
            raw,
            value: Some(looklab_upload::Saturation::Acros(
                looklab_upload::MonochromeFilter::NoFilter,
            )),
        }),
        ..looklab_upload::LookSettings::default()
    }
}

fn mismatched_monochrome_look(raw: i64) -> looklab_upload::LookSettings {
    looklab_upload::LookSettings {
        saturation: Some(looklab_upload::Tagged {
            raw,
            value: Some(looklab_upload::Saturation::Step(0)),
        }),
        ..looklab_upload::LookSettings::default()
    }
}

fn monochrome_look(saturation: i64) -> looklab_upload::LookSettings {
    looklab_upload::LookSettings {
        film_mode: Some(looklab_upload::Tagged {
            raw: 0x0000,
            value: None,
        }),
        saturation: Some(looklab_upload::Tagged {
            raw: saturation,
            value: None,
        }),
        ..looklab_upload::LookSettings::default()
    }
}

fn valid_settings() -> ControlSettings {
    settings_at(&[
        ("color", 0),
        ("color_chrome_effect", 0),
        ("color_chrome_fx_blue", 0),
        ("dynamic_range", 0),
        ("highlight_tone", 0),
        ("shadow_tone", 0),
        ("wb_shift_b", 0),
        ("wb_shift_r", 0),
    ])
}

fn monochrome_settings(filter: i64) -> ControlSettings {
    settings_at(&[
        ("dynamic_range", 0),
        ("highlight_tone", 0),
        ("shadow_tone", 0),
        ("wb_shift_b", 0),
        ("wb_shift_r", 0),
        ("mono_filter", filter),
        ("bw_adjustment", 0),
        ("bw_magenta_green", 0),
        ("grain", 0),
        ("grain_size", 0),
        ("clarity", 0),
        ("noise_reduction", 0),
    ])
}

fn settings_at(entries: &[(&str, i64)]) -> ControlSettings {
    ControlSettings::new(
        entries
            .iter()
            .map(|(key, step)| reproduced(key, *step))
            .collect(),
    )
    .expect("settings")
}

fn with_source(settings: &ControlSettings, entry: ControlEntry) -> ControlSettings {
    let mut entries = settings
        .entries()
        .iter()
        .filter(|candidate| candidate.key != entry.key)
        .cloned()
        .collect::<Vec<_>>();
    entries.push(entry);
    ControlSettings::new(entries).expect("settings")
}

fn highest_snapshot(dir: &Path) -> Option<u64> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let version = path
                .file_name()?
                .to_str()?
                .strip_prefix('v')?
                .parse()
                .ok()?;
            path.is_dir().then_some(version)
        })
        .max()
}

fn copy_shipped_documents(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("shipped documents");
    for entry in std::fs::read_dir(from).expect("shipped documents") {
        let path = entry.expect("shipped entry").path();
        let name = path.file_name().expect("document name");
        if path.is_dir() {
            copy_shipped_documents(&path, &to.join(name));
        } else if path
            .extension()
            .is_some_and(|extension| extension == "yaml")
        {
            std::fs::copy(&path, to.join(name)).expect("shipped document");
        }
    }
}

fn write_snapshot(dir: &Path, version: u64, color: &str) {
    let bodies = dir.join(format!("v{version}/bodies"));
    std::fs::create_dir_all(&bodies).expect("snapshot bodies");
    std::fs::write(
        bodies.join("fujifilm-x-t5.body.yaml"),
        "id: fujifilm/x-t5\nmake: Fujifilm\nmodel: X-T5\nexif_aliases: [X-T5]\n\
         kind: camera\ncapabilities:\n  in_camera_raw_conversion: true\n  \
         jpeg_color_spaces: [srgb]\n  heif: false\nstatus: active\n\
         mapping_status: provisional\n",
    )
    .expect("body entity");
    std::fs::write(bodies.join("fujifilm-x-t5.yaml"), snapshot_schema(color)).expect("schema");
}

/// Keyed by the paths a first snapshot exports.
fn shipped_baseline_files() -> Vec<(String, Vec<u8>)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(root.join("bodies")).expect("shipped bodies") {
        let path = entry.expect("body entry").path();
        if path.extension() != Some(std::ffi::OsStr::new("yaml")) {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("body document");
        let export = if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".body.yaml"))
        {
            let body = looklab_schema::validate_body(&text).expect("body");
            looklab_schema::body_export_path(&body.id)
        } else {
            let schema = looklab_schema::validate_control_schema(&text).expect("schema");
            looklab_schema::schema_export_path(&schema.body)
        };
        files.push((export, text.into_bytes()));
    }
    for entry in std::fs::read_dir(root.join("interfaces")).expect("shipped interfaces") {
        let path = entry.expect("interface entry").path();
        if path.extension() != Some(std::ffi::OsStr::new("yaml")) {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("interface document");
        let profile = looklab_schema::validate_interface_profile(&text).expect("interface");
        files.push((
            looklab_schema::interface_export_path(&profile.id),
            text.into_bytes(),
        ));
    }
    files
}

fn snapshot_schema(color: &str) -> String {
    let mut text = String::from(
        "body: fujifilm/x-t5\nepochs:\n  - id: 1\n    firmware: { from: \"1.00\", to: \"9.99\" }\n    \
         native_sims:\n      - key: classic_chrome\n        look: classic-chrome\n        \
         makernote: { tag: FilmMode, value: 0x0600 }\n    exposure_modes:\n      - key: dr100\n    \
         controls:\n      - key: dynamic_range\n        type: enum\n        \
         values: [dr100, dr200, dr400]\n        default: dr100\n        look_affecting: true\n        \
         affects: [tone, exposure]\n        makernote:\n          tag: DevelopmentDynamicRange\n          \
         values: { dr100: 100, dr200: 200, dr400: 400 }\n      - key: highlight_tone\n        \
         type: decimal\n        range: [-2.0, 4.0]\n        step: 0.5\n        default: 0.0\n        \
         look_affecting: true\n        affects: [tone]\n        \
         makernote: { tag: HighlightTone, encoding: { scale: -16 } }\n      - key: shadow_tone\n        \
         type: decimal\n        range: [-2.0, 4.0]\n        step: 0.5\n        default: 0.0\n        \
         look_affecting: true\n        affects: [tone]\n        \
         makernote: { tag: ShadowTone, encoding: { scale: -16 } }\n",
    );
    text.push_str(color);
    text.push_str(
        "      - key: color_chrome_effect\n        type: enum\n        values: [off, weak, strong]\n        \
         default: off\n        look_affecting: true\n        affects: [chroma]\n        \
         makernote: { tag: ColorChromeEffect, values: { off: 0, weak: 32, strong: 64 } }\n      \
         - key: color_chrome_fx_blue\n        type: enum\n        values: [off, weak, strong]\n        \
         default: off\n        look_affecting: true\n        affects: [chroma]\n        \
         makernote: { tag: ColorChromeFXBlue, values: { off: 0, weak: 32, strong: 64 } }\n      \
         - key: wb_shift_r\n        type: int\n        range: [-9, 9]\n        step: 1\n        \
         default: 0\n        look_affecting: true\n        affects: [wb]\n        \
         makernote: { tag: WhiteBalanceFineTune, component: 0, encoding: { scale: 20 } }\n      \
         - key: wb_shift_b\n        type: int\n        range: [-9, 9]\n        step: 1\n        \
         default: 0\n        look_affecting: true\n        affects: [wb]\n        \
         makernote: { tag: WhiteBalanceFineTune, component: 1, encoding: { scale: 20 } }\n    \
         solve_plan:\n      - stage: tone\n        controls: [highlight_tone]\n",
    );
    text
}

fn unmapped_color() -> &'static str {
    "      - key: color\n        type: int\n        range: [0, 9]\n        step: 1\n        \
     default: 0\n        look_affecting: true\n        affects: [chroma]\n"
}

fn mapped_color() -> &'static str {
    "      - key: color\n        type: int\n        range: [-4, 4]\n        step: 1\n        \
     default: 0\n        look_affecting: true\n        affects: [chroma]\n        makernote:\n          \
     tag: Saturation\n          values: { -4: 0x4e0, -3: 0x4c0, -2: 0x400, -1: 0x180, 0: 0, \
     1: 0x80, 2: 0x100, 3: 0xc0, 4: 0xe0 }\n"
}

fn manifest(settings: ControlSettings) -> (BatchManifest, Vec<Vec<u8>>) {
    manifest_at(CommunitySnapshotVersion::SHIPPED, settings)
}

fn manifest_at(
    version: CommunitySnapshotVersion,
    settings: ControlSettings,
) -> (BatchManifest, Vec<Vec<u8>>) {
    let blobs = vec![sample_blob(), patch_blob()];
    let images = vec![
        image(
            &blobs[0],
            BlobKind::Samples,
            Some(RerenderGroupId::new([7; 16])),
        ),
        image(&blobs[1], BlobKind::PatchTable, None),
    ];
    let manifest = BatchManifest {
        kind: ContributionKind::Library,
        intent: Intent::Native,
        render_class: RenderClass::Colour,
        monochrome_authorization: None,
        body: BodyId::new("fujifilm/x-t5").expect("body id"),
        firmware_version: FirmwareVersionString::new("2.10").expect("firmware"),
        sim_key: SimKey::new("classic_chrome").expect("sim key"),
        community_snapshot_version: version,
        settings,
        images,
    };
    (manifest, blobs)
}

fn send_challenge(nonce: &Nonce, manifest: &BatchManifest) -> Challenge {
    let send = SendManifest::canonical(vec![manifest_hash(manifest)]).expect("send manifest");
    Challenge::for_send(nonce, &send_manifest_hash(&send))
}

fn envelope(
    nonce: Nonce,
    manifest: &BatchManifest,
    blobs: &[Vec<u8>],
    serial: Option<&RawSerial>,
    assertion: &WebAuthnAssertion,
    pow: &PowSolutions,
) -> Vec<u8> {
    let send = SendManifest::canonical(vec![manifest_hash(manifest)]).expect("send manifest");
    encode_envelope(&EnvelopeParts {
        nonce,
        send_manifest: &send,
        manifest,
        assertion,
        pow,
        serial,
        blobs,
    })
}

fn send_envelope(
    nonce: Nonce,
    send: &SendManifest,
    manifest: &BatchManifest,
    blobs: &[Vec<u8>],
    assertion: &WebAuthnAssertion,
) -> Vec<u8> {
    let pow = solve(
        &Challenge::new(&nonce, &manifest_hash(manifest)),
        u32::try_from(manifest.images.len()).expect("image count"),
        &pow_params(),
        0,
    )
    .expect("proof-of-work");
    encode_envelope(&EnvelopeParts {
        nonce,
        send_manifest: send,
        manifest,
        assertion,
        pow: &pow,
        serial: None,
        blobs,
    })
}

fn assertion_from(
    soft: &mut SoftPasskey,
    credential_id: &[u8],
    challenge: &Challenge,
) -> WebAuthnAssertion {
    let options = request_options(challenge.as_bytes(), credential_id, true).public_key;
    let client_data_json = format!(
        r#"{{"type":"webauthn.get","challenge":"{}","origin":"{ORIGIN}"}}"#,
        URL_SAFE_NO_PAD.encode(challenge.as_bytes())
    );
    let hash = Sha256::digest(client_data_json.as_bytes()).to_vec();
    let credential = soft
        .perform_auth(hash, options, 60_000)
        .expect("software authenticator");
    WebAuthnAssertion {
        credential_id: credential.raw_id,
        authenticator_data: credential.response.authenticator_data,
        client_data_json: client_data_json.into_bytes(),
        signature: credential.response.signature,
    }
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

#[derive(Clone, Copy)]
enum RequestDomain {
    Session,
    SessionRevoke,
    Withdraw,
}

impl RequestDomain {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::SessionRevoke => "session-revoke",
            Self::Withdraw => "withdraw",
        }
    }
}

fn request_digest(domain: RequestDomain, payload: &str) -> ManifestHash {
    let mut hasher = Sha256::new();
    hasher.update(b"looklab-request-v1:");
    hasher.update(domain.as_str().as_bytes());
    hasher.update(b":");
    hasher.update(payload.as_bytes());
    ManifestHash::from_bytes(hasher.finalize().into())
}

fn bind_fields(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
        out.extend_from_slice(&(part.len() as u64).to_be_bytes());
        out.extend_from_slice(part);
    }
    out
}

fn action_digest(domain: &str, payload: &[u8]) -> ManifestHash {
    let mut hasher = Sha256::new();
    hasher.update(b"looklab-request-v1:");
    hasher.update(domain.as_bytes());
    hasher.update(b":");
    hasher.update(payload);
    ManifestHash::from_bytes(hasher.finalize().into())
}

fn bad_pow(challenge: &Challenge, n_images: u32) -> PowSolutions {
    for candidate in 0u64..1024 {
        let mut values = [0u64; 16];
        values[0] = candidate;
        let solutions = PowSolutions::new(values);
        if looklab_upload::verify(challenge, n_images, &pow_params(), &solutions).is_err() {
            return solutions;
        }
    }
    panic!("no failing proof-of-work found");
}

#[tokio::test(flavor = "multi_thread")]
async fn happy_path_and_status_feed() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
    let token = body["status_token"].as_str().expect("status token");
    let contribution_id = body["contribution_id"].as_str().expect("contribution id");

    let (status, _, body) = server.get(&format!("/status/{token}")).await;
    assert_eq!(status, StatusCode::OK);
    let feed: Value = serde_json::from_slice(&body).expect("status json");
    assert_eq!(feed["activities"].as_array().expect("activities").len(), 1);
    assert_eq!(feed["activities"][0]["source"], "send");
    let contribution = &feed["activities"][0]["contributions"][0];
    assert_eq!(contribution["id"], contribution_id);
    assert_eq!(contribution["render_class"], "colour");
    assert!(contribution["deferred_until"].is_null());
    assert_eq!(contribution["images"].as_array().expect("images").len(), 2);
    assert_eq!(contribution["images"][0]["status"], "pending");

    let (status, headers, body) = server.get(&format!("/status/{token}.rss")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/rss+xml"))
    );
    let rss = String::from_utf8(body.to_vec()).expect("rss text");
    assert!(rss.contains("<title>Contribution from "), "rss: {rss}");
    assert!(rss.contains("2 photos"), "rss: {rss}");

    let credentials: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM credentials")
        .fetch_one(&server.db)
        .await
        .expect("credentials");
    assert_eq!(credentials, 1);
    let jobs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = 'consistency_score'")
            .fetch_one(&server.db)
            .await
            .expect("jobs");
    assert_eq!(jobs, 1);
    let blobs = std::fs::read_dir(&server.blob_dir)
        .expect("blob dir")
        .count();
    assert_eq!(blobs, 2);

    let credential_json: String = sqlx::query_scalar("SELECT webauthn_credential FROM credentials")
        .fetch_one(&server.db)
        .await
        .expect("credential");
    let passkey: webauthn_rs::prelude::Passkey =
        serde_json::from_str(&credential_json).expect("stored passkey");
    let credential: webauthn_rs::prelude::Credential = passkey.into();
    assert!(credential.counter > 0, "assertion counter was not stored");
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_signature_is_rejected() {
    let mut server = TestServer::start().await;
    let (manifest, blobs) = manifest(valid_settings());
    let nonce = server.nonce().await;
    let pow_challenge = Challenge::new(&nonce, &manifest_hash(&manifest));
    let challenge = send_challenge(&nonce, &manifest);
    let mut assertion = server.assertion(&challenge).await;
    assertion.signature[0] ^= 0xff;
    let pow = solve(&pow_challenge, 2, &pow_params(), 0).expect("pow");
    let bytes = envelope(nonce, &manifest, &blobs, None, &assertion, &pow);
    let (status, body) = server.upload_bytes(bytes).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "SIGNATURE_INVALID");
}

#[tokio::test(flavor = "multi_thread")]
async fn status_token_is_issued_only_once() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED);
    let token = body["status_token"].as_str().expect("token").to_owned();

    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(body["status_token"].is_null(), "second token: {body}");

    let (status, _, body) = server.get(&format!("/status/{token}")).await;
    assert_eq!(status, StatusCode::OK);
    let feed: Value = serde_json::from_slice(&body).expect("status json");
    assert_eq!(feed["activities"].as_array().expect("activities").len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_user_verification_is_rejected() {
    let mut server = TestServer::start().await;
    let (manifest, blobs) = manifest(valid_settings());
    let nonce = server.nonce().await;
    let pow_challenge = Challenge::new(&nonce, &manifest_hash(&manifest));
    let challenge = send_challenge(&nonce, &manifest);
    let assertion = server.assertion_with_verification(&challenge, false).await;
    let pow = solve(&pow_challenge, 2, &pow_params(), 0).expect("pow");
    let bytes = envelope(nonce, &manifest, &blobs, None, &assertion, &pow);
    let (status, body) = server.upload_bytes(bytes).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "SIGNATURE_INVALID");
}

#[tokio::test(flavor = "multi_thread")]
async fn replayed_nonce_is_rejected() {
    let mut server = TestServer::start().await;
    let (manifest, blobs) = manifest(valid_settings());
    let nonce = server.nonce().await;
    let pow_challenge = Challenge::new(&nonce, &manifest_hash(&manifest));
    let challenge = send_challenge(&nonce, &manifest);
    let assertion = server.assertion(&challenge).await;
    let pow = solve(&pow_challenge, 2, &pow_params(), 0).expect("pow");
    let bytes = envelope(nonce, &manifest, &blobs, None, &assertion, &pow);
    let (status, _) = server.upload_bytes(bytes.clone()).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = server.upload_bytes(bytes).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "SEND_REPLAYED");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_parallel_batch_replay_creates_one_contribution() {
    let mut server = TestServer::start().await;
    let (first, first_blobs) = manifest(valid_settings());
    let (mut second, _) = manifest(valid_settings());
    second.sim_key = SimKey::new("provia").expect("sim key");
    let nonce = server.nonce().await;
    let send = SendManifest::canonical(vec![manifest_hash(&first), manifest_hash(&second)])
        .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = server.assertion(&challenge).await;
    let body = send_envelope(nonce, &send, &first, &first_blobs, &assertion);

    let (left, right) = tokio::join!(server.upload_bytes(body.clone()), server.upload_bytes(body));
    let statuses = [left.0, right.0];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::CREATED)
            .count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::CONFLICT
                || **status == StatusCode::SERVICE_UNAVAILABLE)
            .count(),
        1,
        "{statuses:?}"
    );
    let batch_hash = manifest_hash(&first);
    let contributions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM contributions WHERE batch_manifest_hash = ?")
            .bind(batch_hash.as_bytes().as_slice())
            .fetch_one(&server.db)
            .await
            .expect("contributions");
    assert_eq!(contributions, 1);
    let refs: i64 = sqlx::query_scalar("SELECT ref_count FROM blobs WHERE hash = ?")
        .bind(first.images[0].blob_sha256.as_slice())
        .fetch_one(&server.db)
        .await
        .expect("blob refs");
    assert_eq!(refs, 1);
    let jobs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = 'consistency_score'")
            .fetch_one(&server.db)
            .await
            .expect("jobs");
    assert_eq!(jobs, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_persisted_credential_parallel_duplicate_batch_creates_one_contribution() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let (first, first_blobs) = manifest(valid_settings());
    let (mut second, _) = manifest(valid_settings());
    second.sim_key = SimKey::new("provia").expect("sim key");
    let nonce = server.nonce().await;
    let send = SendManifest::canonical(vec![manifest_hash(&first), manifest_hash(&second)])
        .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = server.assertion(&challenge).await;
    let body = send_envelope(nonce, &send, &first, &first_blobs, &assertion);

    let (left, right) = tokio::join!(server.upload_bytes(body.clone()), server.upload_bytes(body));
    let statuses = [left.0, right.0];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::CREATED)
            .count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::CONFLICT
                || **status == StatusCode::SERVICE_UNAVAILABLE)
            .count(),
        1,
        "{statuses:?}"
    );
    let batch_hash = manifest_hash(&first);
    let contributions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM contributions WHERE batch_manifest_hash = ?")
            .bind(batch_hash.as_bytes().as_slice())
            .fetch_one(&server.db)
            .await
            .expect("contributions");
    assert_eq!(contributions, 1);
    let refs: i64 = sqlx::query_scalar("SELECT ref_count FROM blobs WHERE hash = ?")
        .bind(first.images[0].blob_sha256.as_slice())
        .fetch_one(&server.db)
        .await
        .expect("blob refs");
    assert_eq!(refs, 1);
    let jobs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = 'consistency_score'")
            .fetch_one(&server.db)
            .await
            .expect("jobs");
    assert_eq!(jobs, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_send_rejects_a_continuation() {
    let mut server = TestServer::start().await;
    let (first, first_blobs) = manifest(valid_settings());
    let (mut second, second_blobs) = manifest(valid_settings());
    second.sim_key = SimKey::new("provia").expect("sim key");
    let nonce = server.nonce().await;
    let send = SendManifest::canonical(vec![manifest_hash(&first), manifest_hash(&second)])
        .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = server.assertion(&challenge).await;
    let first_body = send_envelope(nonce, &send, &first, &first_blobs, &assertion);
    assert_eq!(server.upload_bytes(first_body).await.0, StatusCode::CREATED);
    sqlx::query("UPDATE send_manifests SET expires_at = 0 WHERE nonce = ?")
        .bind(hex(nonce.as_bytes()))
        .execute(&server.db)
        .await
        .expect("expire send");

    let second_body = send_envelope(nonce, &send, &second, &second_blobs, &assertion);
    let (status, body) = server.upload_bytes(second_body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "SEND_EXPIRED");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_batches_under_a_fresh_nonce_are_a_new_send() {
    let mut server = TestServer::start().await;
    let (first, first_blobs) = manifest(valid_settings());
    let (mut second, second_blobs) = manifest(valid_settings());
    second.sim_key = SimKey::new("provia").expect("sim key");
    for _ in 0..2 {
        let nonce = server.nonce().await;
        let send = SendManifest::canonical(vec![manifest_hash(&first), manifest_hash(&second)])
            .expect("send manifest");
        let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
        let assertion = server.assertion(&challenge).await;
        assert_eq!(
            server
                .upload_bytes(send_envelope(
                    nonce,
                    &send,
                    &first,
                    &first_blobs,
                    &assertion
                ))
                .await
                .0,
            StatusCode::CREATED
        );
        assert_eq!(
            server
                .upload_bytes(send_envelope(
                    nonce,
                    &send,
                    &second,
                    &second_blobs,
                    &assertion
                ))
                .await
                .0,
            StatusCode::CREATED
        );
    }
    let contributions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM contributions")
        .fetch_one(&server.db)
        .await
        .expect("contributions");
    assert_eq!(contributions, 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_credential_cannot_continue_a_bound_send() {
    let mut server = TestServer::start().await;
    let (first, first_blobs) = manifest(valid_settings());
    let (mut second, second_blobs) = manifest(valid_settings());
    second.sim_key = SimKey::new("provia").expect("sim key");
    let nonce = server.nonce().await;
    let send = SendManifest::canonical(vec![manifest_hash(&first), manifest_hash(&second)])
        .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = server.assertion(&challenge).await;
    assert_eq!(
        server
            .upload_bytes(send_envelope(
                nonce,
                &send,
                &first,
                &first_blobs,
                &assertion
            ))
            .await
            .0,
        StatusCode::CREATED
    );

    let mut other = SoftPasskey::new(true);
    let other_id = register(&server.app, &mut other).await;
    let other_assertion = assertion_from(&mut other, &other_id, &challenge);
    let (status, body) = server
        .upload_bytes(send_envelope(
            nonce,
            &send,
            &second,
            &second_blobs,
            &other_assertion,
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"], "SEND_CREDENTIAL_MISMATCH");
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_pow_is_rejected() {
    let mut server = TestServer::start().await;
    let (manifest, blobs) = manifest(valid_settings());
    let nonce = server.nonce().await;
    let pow_challenge = Challenge::new(&nonce, &manifest_hash(&manifest));
    let challenge = send_challenge(&nonce, &manifest);
    let assertion = server.assertion(&challenge).await;
    let pow = bad_pow(&pow_challenge, 2);
    let bytes = envelope(nonce, &manifest, &blobs, None, &assertion, &pow);
    let (status, body) = server.upload_bytes(bytes).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "POW_INVALID");
}

#[tokio::test(flavor = "multi_thread")]
async fn non_canonical_envelope_is_rejected() {
    let mut server = TestServer::start().await;
    let (manifest, blobs) = manifest(valid_settings());
    let nonce = server.nonce().await;
    let pow_challenge = Challenge::new(&nonce, &manifest_hash(&manifest));
    let challenge = send_challenge(&nonce, &manifest);
    let assertion = server.assertion(&challenge).await;
    let pow = solve(&pow_challenge, 2, &pow_params(), 0).expect("pow");
    let mut bytes = envelope(nonce, &manifest, &blobs, None, &assertion, &pow);
    bytes.push(0);
    let (status, body) = server.upload_bytes(bytes).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "MALFORMED_ENVELOPE");
}

#[tokio::test(flavor = "multi_thread")]
async fn schema_invalid_settings_are_rejected() {
    let mut server = TestServer::start().await;
    let (manifest, blobs) = manifest(settings_at(&[("highlight_tone", 0)]));
    let nonce = server.nonce().await;
    let pow_challenge = Challenge::new(&nonce, &manifest_hash(&manifest));
    let challenge = send_challenge(&nonce, &manifest);
    let assertion = server.assertion(&challenge).await;
    let pow = solve(&pow_challenge, 2, &pow_params(), 0).expect("pow");
    let bytes = envelope(nonce, &manifest, &blobs, None, &assertion, &pow);
    let (status, body) = server.upload_bytes(bytes).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "SCHEMA_INVALID");
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_validation_writes_no_blob_files() {
    let mut server = TestServer::start().await;
    let (invalid_settings, settings_blobs) = manifest(settings_at(&[("highlight_tone", 0)]));
    let (status, body) = server
        .upload_manifest(&invalid_settings, &settings_blobs, None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "upload: {body}");
    assert_eq!(body["error"], "SCHEMA_INVALID");

    let (mut unknown_body, body_blobs) = manifest(valid_settings());
    unknown_body.body = BodyId::new("fujifilm/unknown").expect("body id");
    let (status, body) = server
        .upload_manifest(&unknown_body, &body_blobs, None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "upload: {body}");
    assert_eq!(body["error"], "BODY_UNKNOWN");

    assert_eq!(blob_files(&server.blob_dir), 0);
    assert_eq!(staged_rows(&server.db).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_contribution_removes_its_staged_blobs_and_a_retry_succeeds() {
    let mut server = TestServer::start().await;
    sqlx::query(
        "CREATE TRIGGER reject_contribution BEFORE INSERT ON contributions
         BEGIN SELECT RAISE(ABORT, 'reject'); END",
    )
    .execute(&server.db)
    .await
    .expect("trigger");

    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "upload: {body}");
    assert_eq!(body["error"], "INTERNAL");
    assert_eq!(blob_files(&server.blob_dir), 0);
    assert_eq!(staged_rows(&server.db).await, 0);
    assert_eq!(blob_rows(&server.db).await, 0);

    sqlx::query("DROP TRIGGER reject_contribution")
        .execute(&server.db)
        .await
        .expect("drop trigger");
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "retry: {body}");
    assert_eq!(blob_files(&server.blob_dir), 2);
    assert_eq!(staged_rows(&server.db).await, 0);
    assert_eq!(blob_rows(&server.db).await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_blob_reservation_is_rejected_without_writing_files() {
    let mut server = TestServer::start().await;
    sqlx::query(
        "CREATE TRIGGER expire_staging AFTER INSERT ON blob_staging
         BEGIN UPDATE blob_staging SET expires_at = 0 WHERE rowid = NEW.rowid; END",
    )
    .execute(&server.db)
    .await
    .expect("trigger");

    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "upload: {body}");
    assert_eq!(body["error"], "BUSY");
    assert_eq!(blob_files(&server.blob_dir), 0);
    assert_eq!(staged_rows(&server.db).await, 0);

    sqlx::query("DROP TRIGGER expire_staging")
        .execute(&server.db)
        .await
        .expect("drop trigger");
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "retry: {body}");
    assert_eq!(blob_files(&server.blob_dir), 2);
    assert_eq!(staged_rows(&server.db).await, 0);
}

fn blob_files(dir: &Path) -> usize {
    std::fs::read_dir(dir).expect("blob dir").count()
}

async fn staged_rows(db: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM blob_staging")
        .fetch_one(db)
        .await
        .expect("staging rows")
}

async fn blob_rows(db: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM blobs")
        .fetch_one(db)
        .await
        .expect("blob rows")
}

/// A `contribution_images` row: render class, sim, family and mono filter.
type ContributionImageRow = (String, Option<String>, Option<String>, Option<String>);

#[tokio::test(flavor = "multi_thread")]
async fn a_monochrome_batch_is_stored_deferred_with_its_authorization() {
    let mut server = TestServer::start().await;
    let (mut manifest, blobs) = manifest(monochrome_settings(1));
    manifest.body = BodyId::new("fujifilm/x-t4").expect("body id");
    manifest.firmware_version = FirmwareVersionString::new("1.00").expect("firmware");
    manifest.sim_key = SimKey::new("acros_ye").expect("sim key");
    manifest.render_class = RenderClass::Monochrome;
    manifest.monochrome_authorization = Some(MonochromeAuthorization::ModelDevelopment180Days);
    for image in &mut manifest.images {
        image.meta.look = Some(monochrome_look(0x0502));
    }
    let (status, body) = server.upload_manifest(&manifest, &blobs, None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");

    let row: (String, Option<String>, Option<i64>, String, i64) = sqlx::query_as(
        "SELECT render_class, monochrome_authorization, deferred_until, status, created_at
         FROM contributions",
    )
    .fetch_one(&server.db)
    .await
    .expect("contribution");
    assert_eq!(row.0, "monochrome");
    assert_eq!(row.1.as_deref(), Some("model_development_180_days"));
    let deadline = row.2.expect("deferred deadline");
    assert_eq!(deadline - row.4, 180 * 86_400);
    assert_eq!(row.3, "pending");

    let images: Vec<ContributionImageRow> = sqlx::query_as(
        "SELECT render_class, native_sim, family, mono_filter FROM contribution_images",
    )
    .fetch_all(&server.db)
    .await
    .expect("images");
    assert_eq!(images.len(), 2);
    for (render_class, native_sim, family, filter) in images {
        assert_eq!(render_class, "monochrome");
        assert_eq!(native_sim.as_deref(), Some("acros_ye"));
        assert_eq!(family.as_deref(), Some("acros"));
        assert_eq!(filter.as_deref(), Some("yellow"));
    }

    let jobs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = 'consistency_score'")
            .fetch_one(&server.db)
            .await
            .expect("jobs");
    assert_eq!(jobs, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deferred_batch_must_declare_its_spatial_controls() {
    let mut server = TestServer::start().await;
    let incomplete = settings_at(&[
        ("dynamic_range", 0),
        ("highlight_tone", 0),
        ("shadow_tone", 0),
        ("wb_shift_b", 0),
        ("wb_shift_r", 0),
        ("mono_filter", 1),
        ("bw_adjustment", 0),
        ("bw_magenta_green", 0),
        ("clarity", 0),
        ("noise_reduction", 0),
    ]);
    let (mut manifest, blobs) = manifest(incomplete);
    manifest.body = BodyId::new("fujifilm/x-t4").expect("body id");
    manifest.firmware_version = FirmwareVersionString::new("1.00").expect("firmware");
    manifest.sim_key = SimKey::new("acros_ye").expect("sim key");
    manifest.render_class = RenderClass::Monochrome;
    manifest.monochrome_authorization = Some(MonochromeAuthorization::ModelDevelopment180Days);
    for image in &mut manifest.images {
        image.meta.look = Some(monochrome_look(0x0502));
    }
    let (status, body) = server.upload_manifest(&manifest, &blobs, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "upload: {body}");
    assert_eq!(body["error"], "SCHEMA_INVALID");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_monochrome_image_rejects_a_colour_sim_declaration() {
    let mut server = TestServer::start().await;
    let (mut manifest, blobs) = manifest(valid_settings());
    manifest.body = BodyId::new("fujifilm/x-t4").expect("body id");
    manifest.firmware_version = FirmwareVersionString::new("1.00").expect("firmware");
    manifest.sim_key = SimKey::new("provia").expect("sim key");
    for image in &mut manifest.images {
        image.meta.look = Some(monochrome_look(0x0500));
    }
    let (status, body) = server.upload_manifest(&manifest, &blobs, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "upload: {body}");
    assert_eq!(body["error"], "RENDER_CLASS_MISMATCH");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_monochrome_signed_batch_rejects_colour_metadata() {
    let mut server = TestServer::start().await;
    let (mut manifest, blobs) = manifest(monochrome_settings(0));
    manifest.body = BodyId::new("fujifilm/x-t4").expect("body id");
    manifest.firmware_version = FirmwareVersionString::new("1.00").expect("firmware");
    manifest.sim_key = SimKey::new("acros").expect("sim key");
    manifest.render_class = RenderClass::Monochrome;
    manifest.monochrome_authorization = Some(MonochromeAuthorization::ModelDevelopment180Days);
    for image in &mut manifest.images {
        image.meta.look = Some(looklab_upload::LookSettings {
            film_mode: Some(looklab_upload::Tagged {
                raw: 0x0000,
                value: None,
            }),
            ..looklab_upload::LookSettings::default()
        });
    }
    let (status, body) = server.upload_manifest(&manifest, &blobs, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "upload: {body}");
    assert_eq!(body["error"], "RENDER_CLASS_MISMATCH");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_declared_look_affecting_control_is_accepted_and_stored() {
    let mut server = TestServer::start().await;
    let settings = with_source(&valid_settings(), declared("color", 1));
    let (manifest, blobs) = manifest(settings);
    let (status, body) = server.upload_manifest(&manifest, &blobs, None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");

    let rows: Vec<(String,)> = sqlx::query_as("SELECT setting_source FROM contribution_images")
        .fetch_all(&server.db)
        .await
        .expect("setting sources");
    assert_eq!(rows.len(), 2);
    for (text,) in rows {
        let pairs: Vec<(String, String)> = serde_json::from_str(&text).expect("source pairs");
        assert!(
            pairs
                .iter()
                .any(|(key, source)| key == "color" && source == "declared"),
            "{pairs:?}"
        );
        assert!(
            pairs
                .iter()
                .any(|(key, source)| key == "highlight_tone" && source == "reproduced"),
            "{pairs:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_look_affecting_control_is_rejected_unless_it_is_a_card() {
    let mut server = TestServer::start().await;
    let incomplete = settings_at(&[
        ("color_chrome_effect", 0),
        ("color_chrome_fx_blue", 0),
        ("dynamic_range", 0),
        ("highlight_tone", 0),
        ("shadow_tone", 0),
        ("wb_shift_b", 0),
        ("wb_shift_r", 0),
    ]);
    let (undeclared, blobs) = manifest(incomplete.clone());
    let (status, body) = server.upload_manifest(&undeclared, &blobs, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "upload: {body}");
    assert_eq!(body["error"], "SCHEMA_INVALID");

    let (mut card, blobs) = manifest(incomplete);
    let tag = render_tag();
    card.intent = Intent::Card(CardDeclaration::Tagged(tag.clone()));
    for image in &mut card.images {
        image.meta.render_tag = Some(tag.clone());
    }
    let (status, body) = server.upload_manifest(&card, &blobs, None).await;
    assert_eq!(status, StatusCode::CREATED, "card upload: {body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_manifest_snapshot_version_decides_which_settings_are_reproducible() {
    let mut server = TestServer::start_with_community(|dir| {
        write_snapshot(dir, 2, unmapped_color());
        write_snapshot(dir, 3, mapped_color());
    })
    .await;

    let declared_color = with_source(&valid_settings(), declared("color", 7));
    let (old, blobs) = manifest_at(CommunitySnapshotVersion::new(2), declared_color.clone());
    let (status, body) = server.upload_manifest(&old, &blobs, None).await;
    assert_eq!(status, StatusCode::CREATED, "v2 upload: {body}");

    let (current, blobs) = manifest_at(CommunitySnapshotVersion::new(3), declared_color);
    let (status, body) = server.upload_manifest(&current, &blobs, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "v3 upload: {body}");
    assert_eq!(body["error"], "SCHEMA_INVALID");

    let (stale, blobs) = manifest_at(CommunitySnapshotVersion::new(1), valid_settings());
    let (status, body) = server.upload_manifest(&stale, &blobs, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "v1 upload: {body}");
    assert_eq!(body["error"], "SNAPSHOT_TOO_OLD");

    let (unknown, blobs) = manifest_at(CommunitySnapshotVersion::new(9), valid_settings());
    let (status, body) = server.upload_manifest(&unknown, &blobs, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "v9 upload: {body}");
    assert_eq!(body["error"], "SNAPSHOT_UNKNOWN");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reproduced_claim_for_an_unmapped_control_is_rejected() {
    let mut server = TestServer::start_with_community(|dir| {
        write_snapshot(dir, 2, unmapped_color());
    })
    .await;
    let settings = settings_at(&[
        ("color", 1),
        ("color_chrome_effect", 0),
        ("color_chrome_fx_blue", 0),
        ("dynamic_range", 0),
        ("highlight_tone", 0),
        ("shadow_tone", 0),
        ("wb_shift_b", 0),
        ("wb_shift_r", 0),
    ]);
    let (manifest, blobs) = manifest_at(CommunitySnapshotVersion::new(2), settings);
    let (status, body) = server.upload_manifest(&manifest, &blobs, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "upload: {body}");
    assert_eq!(body["error"], "SCHEMA_INVALID");
}

#[tokio::test(flavor = "multi_thread")]
async fn old_schema_monochrome_metadata_cannot_enter_the_colour_pipeline() {
    let mut server = TestServer::start_with_community(|dir| {
        write_snapshot(dir, 2, mapped_color());
    })
    .await;
    for look in [
        monochrome_look(0x0500),
        typed_monochrome_look(0x0500),
        mismatched_monochrome_look(0x0500),
    ] {
        let (mut manifest, blobs) = manifest_at(CommunitySnapshotVersion::new(2), valid_settings());
        for image in &mut manifest.images {
            image.meta.look = Some(look.clone());
        }
        let (status, body) = server.upload_manifest(&manifest, &blobs, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "upload: {body}");
        assert_eq!(body["error"], "RENDER_CLASS_MISMATCH");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_monochrome_signed_sim_rejects_contradicting_metadata() {
    let mut server = TestServer::start().await;
    let (mut manifest, blobs) = manifest(monochrome_settings(0));
    manifest.body = BodyId::new("fujifilm/x-t4").expect("body id");
    manifest.firmware_version = FirmwareVersionString::new("1.00").expect("firmware");
    manifest.sim_key = SimKey::new("sepia").expect("sim key");
    manifest.render_class = RenderClass::Monochrome;
    manifest.monochrome_authorization = Some(MonochromeAuthorization::ModelDevelopment180Days);
    for image in &mut manifest.images {
        image.meta.look = Some(monochrome_look(0x0501));
    }
    let (status, body) = server.upload_manifest(&manifest, &blobs, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "upload: {body}");
    assert_eq!(body["error"], "RENDER_CLASS_MISMATCH");
}

#[tokio::test(flavor = "multi_thread")]
async fn card_intent_is_stored_with_its_parsed_fields() {
    let mut server = TestServer::start().await;
    let (mut manifest, blobs) = manifest(valid_settings());
    let tag = render_tag();
    manifest.intent = Intent::Card(CardDeclaration::Tagged(tag.clone()));
    manifest.images[0].meta.render_tag = Some(tag);

    let (status, body) = server.upload_manifest(&manifest, &blobs, None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
    let id = body["contribution_id"]
        .as_str()
        .expect("contribution id")
        .to_owned();

    let row: (String, Option<String>) =
        sqlx::query_as("SELECT intent, card FROM contributions WHERE id = ?")
            .bind(&id)
            .fetch_one(&server.db)
            .await
            .expect("contribution row");
    assert_eq!(row.0, "card");
    let declaration: CardDeclaration =
        serde_json::from_str(row.1.as_deref().expect("card field")).expect("card json");
    assert_eq!(declaration, CardDeclaration::Tagged(render_tag()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deferred_monochrome_card_keeps_card_intent_and_stock_metadata() {
    let mut server = TestServer::start().await;
    let (mut manifest, blobs) = manifest(monochrome_settings(1));
    manifest.body = BodyId::new("fujifilm/x-t4").expect("body id");
    manifest.firmware_version = FirmwareVersionString::new("1.00").expect("firmware");
    manifest.sim_key = SimKey::new("provia").expect("stock sim key");
    manifest.render_class = RenderClass::Monochrome;
    manifest.monochrome_authorization = Some(MonochromeAuthorization::ModelDevelopment180Days);
    let tag = monochrome_render_tag();
    manifest.intent = Intent::Card(CardDeclaration::Tagged(tag.clone()));
    for image in &mut manifest.images {
        image.meta.render_tag = Some(tag.clone());
        image.meta.look = Some(looklab_upload::LookSettings {
            film_mode: Some(looklab_upload::Tagged {
                raw: 0x0000,
                value: None,
            }),
            ..looklab_upload::LookSettings::default()
        });
    }
    let (status, body) = server.upload_manifest(&manifest, &blobs, None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
    let token = body["status_token"].as_str().expect("token");

    let row: (String, String, Option<String>) =
        sqlx::query_as("SELECT intent, render_class, monochrome_authorization FROM contributions")
            .fetch_one(&server.db)
            .await
            .expect("contribution");
    assert_eq!(row.0, "card");
    assert_eq!(row.1, "monochrome");
    assert_eq!(row.2.as_deref(), Some("model_development_180_days"));

    let images: Vec<(String, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT render_class, native_sim, family FROM contribution_images")
            .fetch_all(&server.db)
            .await
            .expect("images");
    for (render_class, native_sim, family) in images {
        assert_eq!(render_class, "monochrome");
        assert_eq!(native_sim.as_deref(), Some("provia"));
        assert_eq!(family.as_deref(), Some("acros"));
    }

    let (status, _, body) = server.get(&format!("/status/{token}")).await;
    assert_eq!(status, StatusCode::OK);
    let feed: Value = serde_json::from_slice(&body).expect("status json");
    let contribution = &feed["activities"][0]["contributions"][0];
    assert_eq!(contribution["render_class"], "monochrome");
    assert!(contribution["deferred_until"].as_i64().is_some());
    assert_eq!(contribution["status"], "pending");
    let (_, _, rss) = server.get(&format!("/status/{token}.rss")).await;
    let rss = String::from_utf8(rss.to_vec()).expect("rss");
    assert!(
        rss.contains("received, awaiting monochrome analysis"),
        "{rss}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_card_tag_under_native_is_rejected() {
    let mut server = TestServer::start().await;
    let (mut manifest, blobs) = manifest(valid_settings());
    manifest.images[0].meta.render_tag_malformed = true;

    let (status, body) = server.upload_manifest(&manifest, &blobs, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "MALFORMED_ENVELOPE");
}

#[tokio::test(flavor = "multi_thread")]
async fn card_fields_must_match_the_images() {
    let mut server = TestServer::start().await;
    let (mut manifest, blobs) = manifest(valid_settings());
    manifest.images[0].meta.render_tag = Some(render_tag());

    let (status, body) = server.upload_manifest(&manifest, &blobs, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "MALFORMED_ENVELOPE");
}

fn render_tag() -> looklab_upload::RenderTag {
    parse_render_tag("looklab-card/1 fw=acme-sim/2.0.1 look=provia@1.0.0 cid=0123456789abcdef")
        .expect("render tag")
}

fn monochrome_render_tag() -> looklab_upload::RenderTag {
    parse_render_tag("looklab-card/1 fw=acme-sim/2.0.1 look=acros@1.0.0 cid=0123456789abcdef")
        .expect("render tag")
}

#[tokio::test(flavor = "multi_thread")]
async fn withdrawal_marks_contribution_and_schedules_deletion() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
    let token = body["status_token"].as_str().expect("token").to_owned();
    let contribution_id = body["contribution_id"]
        .as_str()
        .expect("contribution")
        .to_owned();

    let nonce = server.nonce().await;
    let digest = request_digest(RequestDomain::Withdraw, &contribution_id);
    let challenge = Challenge::new(&nonce, &digest);
    let assertion = server.assertion(&challenge).await;
    let payload = json!({
        "nonce": b64(nonce.as_bytes()),
        "credential_id": b64(&assertion.credential_id),
        "authenticator_data": b64(&assertion.authenticator_data),
        "client_data_json": b64(&assertion.client_data_json),
        "signature": b64(&assertion.signature),
        "contribution_ids": [contribution_id],
    });
    let (status, body) = post_json(&server.app, "/api/withdraw", &payload).await;
    assert_eq!(status, StatusCode::OK, "withdraw: {body}");
    let delete_after = body["delete_after"].as_i64().expect("delete_after");

    let feed: Value = serde_json::from_slice(&server.get(&format!("/status/{token}")).await.2)
        .expect("status json");
    assert_eq!(
        feed["activities"][0]["contributions"][0]["status"],
        "withdrawn"
    );

    let (job_kind, run_after): (String, i64) =
        sqlx::query_as("SELECT kind, run_after FROM jobs WHERE kind = 'hard_delete_blobs'")
            .fetch_one(&server.db)
            .await
            .expect("deletion job");
    assert_eq!(job_kind, "hard_delete_blobs");
    assert_eq!(run_after, delete_after);
    let withdrawals: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM withdrawals")
        .fetch_one(&server.db)
        .await
        .expect("withdrawals");
    assert_eq!(withdrawals, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_profile_cannot_use_any_intake_route_but_can_withdraw() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
    let token = body["status_token"].as_str().expect("token").to_owned();
    let contribution_id = body["contribution_id"]
        .as_str()
        .expect("contribution")
        .to_owned();
    let credential = server.credential_uuid().await;
    let moderator = server.moderator_token().await;
    let body_yaml = include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml");
    let (status, body) = server.submit_proposal("body", body_yaml, &[], true).await;
    assert_eq!(status, StatusCode::CREATED, "pre-block proposal: {body}");
    let proposal = body["view"]["id"].as_str().expect("proposal id").to_owned();
    let before: (String, String, f64) =
        sqlx::query_as("SELECT tier, roles, reputation FROM credentials WHERE id = ?")
            .bind(&credential)
            .fetch_one(&server.db)
            .await
            .expect("credential before");
    let (status, blocked) = post_json_with_token(
        &server.app,
        "/api/mod/credential/block",
        &moderator,
        &json!({"credential": credential.clone(), "reason": "abusive submissions"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "block: {blocked}");
    let after: (String, String, f64) =
        sqlx::query_as("SELECT tier, roles, reputation FROM credentials WHERE id = ?")
            .bind(&credential)
            .fetch_one(&server.db)
            .await
            .expect("credential after");
    assert_eq!(after, before);
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE type = 'credential_intake_blocked'",
    )
    .fetch_one(&server.db)
    .await
    .expect("block audit");
    assert_eq!(audits, 1);
    let payload: String = sqlx::query_scalar(
        "SELECT payload FROM audit_log WHERE type = 'credential_intake_blocked' LIMIT 1",
    )
    .fetch_one(&server.db)
    .await
    .expect("block audit payload");
    let payload: Value = serde_json::from_str(&payload).expect("block audit json");
    assert_eq!(payload["subject"].as_str(), Some(credential.as_str()));
    assert_eq!(payload["reason"].as_str(), Some("abusive submissions"));
    let (status, repeated) = post_json_with_token(
        &server.app,
        "/api/mod/credential/block",
        &moderator,
        &json!({"credential": credential.clone(), "reason": "again"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "repeat block: {repeated}");
    assert_eq!(repeated["blocked_at"], blocked["blocked_at"]);
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE type = 'credential_intake_blocked'",
    )
    .fetch_one(&server.db)
    .await
    .expect("block audit after repeat");
    assert_eq!(audits, 1);

    let contributions_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM contributions")
        .fetch_one(&server.db)
        .await
        .expect("contributions before");
    let unused_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM nonces WHERE used_at IS NULL")
            .fetch_one(&server.db)
            .await
            .expect("unused nonces before");
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "upload: {body}");
    assert_eq!(body["error"], "CREDENTIAL_BLOCKED");
    let contributions_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM contributions")
        .fetch_one(&server.db)
        .await
        .expect("contributions after");
    assert_eq!(contributions_after, contributions_before);
    let unused_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nonces WHERE used_at IS NULL")
        .fetch_one(&server.db)
        .await
        .expect("unused nonces after");
    assert_eq!(unused_after, unused_before + 1);
    let staged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blob_staging")
        .fetch_one(&server.db)
        .await
        .expect("staged blobs");
    assert_eq!(staged, 0);

    let (status, body) = server.submit_proposal("body", body_yaml, &[], true).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "proposal: {body}");
    assert_eq!(body["error"], "CREDENTIAL_BLOCKED");

    let tags = vec![LabeledTag {
        tag: "FilmMode".try_into().expect("tag"),
        value: MakernoteValue(0x0700),
        label: "classic_chrome".try_into().expect("label"),
    }];
    let (status, body) = server.confirm_proposal(&proposal, tags).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "confirmation: {body}");
    assert_eq!(body["error"], "CREDENTIAL_BLOCKED");

    let cube = lut_test_cube();
    let bytes = cube.as_bytes();
    let digest = hex(&Sha256::digest(bytes));
    let payload = format!(
        "look: blocked-lut\nname: Blocked LUT\nlicense: {{ own_work: true }}\n\
         attribution: A contributor\nlut:\n  file: blocked-lut.cube\n  sha256: {digest}\n  \
         size: {}\nencoding: flog2_fgamut\n",
        bytes.len()
    );
    let (status, body) = server.submit_lut("lut_look", &payload, bytes).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "lut proposal: {body}");
    assert_eq!(body["error"], "CREDENTIAL_BLOCKED");

    let (status, body) = server
        .donate_fixture("blocked.raf", b"blocked fixture")
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "fixture: {body}");
    assert_eq!(body["error"], "CREDENTIAL_BLOCKED");

    let camera = documented_camera("fujifilm/x-t5");
    let confirmation = confirmation_for(&camera, "native_simulations", "2.10");
    let (status, body) = post_standalone_confirmation_response(&mut server, &confirmation).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "confirmation: {body}");
    assert_eq!(body["error"], "CREDENTIAL_BLOCKED");

    let active: String = sqlx::query_scalar("SELECT status FROM credentials WHERE id = ?")
        .bind(&credential)
        .fetch_one(&server.db)
        .await
        .expect("credential status");
    assert_eq!(active, "active");

    let nonce = server.nonce().await;
    let digest = request_digest(RequestDomain::Withdraw, &contribution_id);
    let challenge = Challenge::new(&nonce, &digest);
    let assertion = server.assertion(&challenge).await;
    let payload = json!({
        "nonce": b64(nonce.as_bytes()),
        "credential_id": b64(&assertion.credential_id),
        "authenticator_data": b64(&assertion.authenticator_data),
        "client_data_json": b64(&assertion.client_data_json),
        "signature": b64(&assertion.signature),
        "contribution_ids": [contribution_id],
    });
    let (status, body) = post_json(&server.app, "/api/withdraw", &payload).await;
    assert_eq!(status, StatusCode::OK, "withdraw: {body}");
    let feed: Value = serde_json::from_slice(&server.get(&format!("/status/{token}")).await.2)
        .expect("status json");
    assert_eq!(
        feed["activities"][0]["contributions"][0]["status"],
        "withdrawn"
    );
}

async fn upload_two_batch_send(server: &mut TestServer) -> (String, Vec<String>) {
    let (first, first_blobs) = manifest(valid_settings());
    let (mut second, second_blobs) = manifest(valid_settings());
    second.sim_key = SimKey::new("provia").expect("sim key");
    let nonce = server.nonce().await;
    let send = SendManifest::canonical(vec![manifest_hash(&first), manifest_hash(&second)])
        .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = server.assertion(&challenge).await;
    let (status, first_body) = server
        .upload_bytes(send_envelope(
            nonce,
            &send,
            &first,
            &first_blobs,
            &assertion,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "first batch: {first_body}");
    let token = first_body["status_token"]
        .as_str()
        .expect("token")
        .to_owned();
    let (status, second_body) = server
        .upload_bytes(send_envelope(
            nonce,
            &send,
            &second,
            &second_blobs,
            &assertion,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "second batch: {second_body}");
    let ids = vec![
        first_body["contribution_id"]
            .as_str()
            .expect("first contribution")
            .to_owned(),
        second_body["contribution_id"]
            .as_str()
            .expect("second contribution")
            .to_owned(),
    ];
    (token, ids)
}

async fn withdraw_payload(server: &mut TestServer, ids: &[String], signed: &[String]) -> Value {
    let mut canonical = signed.to_vec();
    canonical.sort();
    canonical.dedup();
    let nonce = server.nonce().await;
    let digest = request_digest(RequestDomain::Withdraw, &canonical.join("\n"));
    let challenge = Challenge::new(&nonce, &digest);
    let assertion = server.assertion(&challenge).await;
    json!({
        "nonce": b64(nonce.as_bytes()),
        "credential_id": b64(&assertion.credential_id),
        "authenticator_data": b64(&assertion.authenticator_data),
        "client_data_json": b64(&assertion.client_data_json),
        "signature": b64(&assertion.signature),
        "contribution_ids": ids,
    })
}

async fn assert_nothing_withdrawn(server: &TestServer, token: &str, expected: usize) {
    let feed: Value = serde_json::from_slice(&server.get(&format!("/status/{token}")).await.2)
        .expect("status json");
    let contributions = feed["activities"]
        .as_array()
        .expect("activities")
        .iter()
        .flat_map(|activity| activity["contributions"].as_array().expect("contributions"))
        .collect::<Vec<_>>();
    assert_eq!(contributions.len(), expected);
    for contribution in contributions {
        assert_ne!(contribution["status"], "withdrawn");
    }
    let withdrawals: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM withdrawals")
        .fetch_one(&server.db)
        .await
        .expect("withdrawals");
    assert_eq!(withdrawals, 0);
    let jobs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = 'hard_delete_blobs'")
            .fetch_one(&server.db)
            .await
            .expect("deletion jobs");
    assert_eq!(jobs, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_multi_batch_send_reads_as_one_submission() {
    let mut server = TestServer::start().await;
    let (token, ids) = upload_two_batch_send(&mut server).await;

    let feed: Value = serde_json::from_slice(&server.get(&format!("/status/{token}")).await.2)
        .expect("status json");
    let activities = feed["activities"].as_array().expect("activities");
    assert_eq!(activities.len(), 1);
    assert_eq!(activities[0]["source"], "send");
    assert_eq!(
        activities[0]["contributions"]
            .as_array()
            .expect("contributions")
            .len(),
        2
    );

    sqlx::query("UPDATE contributions SET body_id = 'fujifilm/x-s10' WHERE id = ?")
        .bind(&ids[1])
        .execute(&server.db)
        .await
        .expect("second camera");
    let feed: Value = serde_json::from_slice(&server.get(&format!("/status/{token}")).await.2)
        .expect("status json");
    let activities = feed["activities"].as_array().expect("activities");
    assert_eq!(activities.len(), 1, "one send stays one submission");
    let bodies = activities[0]["contributions"]
        .as_array()
        .expect("contributions")
        .iter()
        .map(|contribution| contribution["body_id"].as_str().expect("body"))
        .collect::<Vec<_>>();
    assert!(bodies.contains(&"fujifilm/x-t5"));
    assert!(bodies.contains(&"fujifilm/x-s10"));
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_records_group_by_day_and_camera() {
    let mut server = TestServer::start().await;
    let (status, first) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED);
    let token = first["status_token"].as_str().expect("token").to_owned();
    let first_id = first["contribution_id"].as_str().expect("contribution");
    let (status, second) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED);
    let second_id = second["contribution_id"].as_str().expect("contribution");

    sqlx::query(
        "UPDATE contributions SET send_nonce = NULL, created_at = 1_700_000_000 WHERE id IN (?, ?)",
    )
    .bind(first_id)
    .bind(second_id)
    .execute(&server.db)
    .await
    .expect("legacy records");

    let feed: Value = serde_json::from_slice(&server.get(&format!("/status/{token}")).await.2)
        .expect("status json");
    let activities = feed["activities"].as_array().expect("activities");
    assert_eq!(activities.len(), 1);
    assert_eq!(activities[0]["source"], "activity");
    assert_eq!(
        activities[0]["contributions"]
            .as_array()
            .expect("contributions")
            .len(),
        2
    );

    sqlx::query("UPDATE contributions SET body_id = 'fujifilm/x-s10' WHERE id = ?")
        .bind(second_id)
        .execute(&server.db)
        .await
        .expect("second camera");
    let feed: Value = serde_json::from_slice(&server.get(&format!("/status/{token}")).await.2)
        .expect("status json");
    assert_eq!(
        feed["activities"].as_array().expect("activities").len(),
        2,
        "different cameras are different activity"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn group_withdrawal_binds_the_selection_and_checks_every_item() {
    let mut server = TestServer::start().await;
    let (token, ids) = upload_two_batch_send(&mut server).await;
    let mut sorted = ids.clone();
    sorted.sort();

    let payload = withdraw_payload(&mut server, &ids, std::slice::from_ref(&sorted[0])).await;
    let (status, body) = post_json(&server.app, "/api/withdraw", &payload).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"], "SIGNATURE_INVALID");
    assert_nothing_withdrawn(&server, &token, 2).await;

    let missing = "ffffffff-ffff-4fff-bfff-ffffffffffff".to_owned();
    let requested = vec![sorted[0].clone(), missing];
    let payload = withdraw_payload(&mut server, &requested, &requested).await;
    let (status, body) = post_json(&server.app, "/api/withdraw", &payload).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], "CONTRIBUTION_UNKNOWN");
    assert_nothing_withdrawn(&server, &token, 2).await;

    let mut unsorted = ids.clone();
    unsorted.reverse();
    let payload = withdraw_payload(&mut server, &unsorted, &sorted).await;
    let (status, body) = post_json(&server.app, "/api/withdraw", &payload).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let feed: Value = serde_json::from_slice(&server.get(&format!("/status/{token}")).await.2)
        .expect("status json");
    let contributions = feed["activities"]
        .as_array()
        .expect("activities")
        .iter()
        .flat_map(|activity| activity["contributions"].as_array().expect("contributions"))
        .collect::<Vec<_>>();
    assert_eq!(contributions.len(), 2);
    for contribution in contributions {
        assert_eq!(contribution["status"], "withdrawn");
    }
    let withdrawals: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM withdrawals")
        .fetch_one(&server.db)
        .await
        .expect("withdrawals");
    assert_eq!(withdrawals, 2);
    let jobs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = 'hard_delete_blobs'")
            .fetch_one(&server.db)
            .await
            .expect("deletion jobs");
    assert_eq!(jobs, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn group_withdrawal_treats_expired_deferred_members_as_terminal() {
    let mut server = TestServer::start().await;
    let (status, colour) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "{colour}");
    let token = colour["status_token"].as_str().expect("token").to_owned();
    let colour_id = colour["contribution_id"]
        .as_str()
        .expect("colour contribution")
        .to_owned();
    let (status, expired) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "{expired}");
    let expired_id = expired["contribution_id"]
        .as_str()
        .expect("expired contribution")
        .to_owned();

    let send_nonce = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    sqlx::query("UPDATE contributions SET send_nonce = ? WHERE id = ?")
        .bind(send_nonce)
        .bind(&colour_id)
        .execute(&server.db)
        .await
        .expect("colour send");
    sqlx::query(
        "UPDATE contributions
         SET send_nonce = ?, render_class = 'monochrome', status = 'withdrawn', deferred_until = 1
         WHERE id = ?",
    )
    .bind(send_nonce)
    .bind(&expired_id)
    .execute(&server.db)
    .await
    .expect("expired send");

    let feed: Value = serde_json::from_slice(&server.get(&format!("/status/{token}")).await.2)
        .expect("status json");
    let activities = feed["activities"].as_array().expect("activities");
    assert_eq!(activities.len(), 1);
    assert_eq!(
        activities[0]["contributions"]
            .as_array()
            .expect("contributions")
            .len(),
        2
    );

    let ids = vec![colour_id.clone(), expired_id.clone()];
    let payload = withdraw_payload(&mut server, &ids, &ids).await;
    let (status, body) = post_json(&server.app, "/api/withdraw", &payload).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let feed: Value = serde_json::from_slice(&server.get(&format!("/status/{token}")).await.2)
        .expect("status json");
    let contributions = feed["activities"]
        .as_array()
        .expect("activities")
        .iter()
        .flat_map(|activity| activity["contributions"].as_array().expect("contributions"))
        .collect::<Vec<_>>();
    assert_eq!(contributions.len(), 2);
    for contribution in contributions {
        assert_eq!(contribution["status"], "withdrawn");
    }
    let withdrawals: Vec<String> = sqlx::query_scalar("SELECT contribution_id FROM withdrawals")
        .fetch_all(&server.db)
        .await
        .expect("withdrawals");
    assert_eq!(withdrawals, [colour_id.as_str()]);
    let jobs: Vec<String> =
        sqlx::query_scalar("SELECT payload FROM jobs WHERE kind = 'hard_delete_blobs'")
            .fetch_all(&server.db)
            .await
            .expect("deletion jobs");
    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].contains(colour_id.as_str()), "{jobs:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn rss_activity_guid_survives_a_new_batch() {
    let mut server = TestServer::start().await;
    let (first, first_blobs) = manifest(valid_settings());
    let (mut second, second_blobs) = manifest(valid_settings());
    second.sim_key = SimKey::new("provia").expect("sim key");
    let nonce = server.nonce().await;
    let send = SendManifest::canonical(vec![manifest_hash(&first), manifest_hash(&second)])
        .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = server.assertion(&challenge).await;
    let (status, first_body) = server
        .upload_bytes(send_envelope(
            nonce,
            &send,
            &first,
            &first_blobs,
            &assertion,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{first_body}");
    let token = first_body["status_token"]
        .as_str()
        .expect("token")
        .to_owned();
    let first_rss = String::from_utf8(server.get(&format!("/status/{token}.rss")).await.2.to_vec())
        .expect("rss");
    let first_guid = rss_guid(&first_rss);

    let (status, second_body) = server
        .upload_bytes(send_envelope(
            nonce,
            &send,
            &second,
            &second_blobs,
            &assertion,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{second_body}");
    let second_rss =
        String::from_utf8(server.get(&format!("/status/{token}.rss")).await.2.to_vec())
            .expect("rss");
    let second_guid = rss_guid(&second_rss);

    assert_eq!(first_guid, second_guid);
    assert_eq!(second_rss.matches("<item>").count(), 1);
}

fn rss_guid(xml: &str) -> String {
    const START: &str = "<guid isPermaLink=\"false\">";
    let start = xml.find(START).expect("guid start") + START.len();
    let end = xml[start..].find("</guid>").expect("guid end") + start;
    xml[start..end].to_owned()
}

async fn signed_session_request(server: &mut TestServer, domain: RequestDomain) -> Value {
    let nonce = server.nonce().await;
    let challenge = Challenge::new(&nonce, &request_digest(domain, ""));
    let assertion = server.assertion(&challenge).await;
    json!({
        "nonce": b64(nonce.as_bytes()),
        "credential_id": b64(&assertion.credential_id),
        "authenticator_data": b64(&assertion.authenticator_data),
        "client_data_json": b64(&assertion.client_data_json),
        "signature": b64(&assertion.signature),
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_coexist_and_revoke_every_token() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED);
    let mut tokens = vec![body["status_token"].as_str().expect("token").to_owned()];
    for _ in 0..2 {
        let payload = signed_session_request(&mut server, RequestDomain::Session).await;
        let (status, body) = post_json(&server.app, "/api/session", &payload).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["roles"], json!([]));
        assert_eq!(body["tier"], "new");
        let token = body["status_token"].as_str().expect("token").to_owned();
        assert!(!tokens.contains(&token));
        tokens.push(token);
    }
    for token in &tokens {
        assert_eq!(
            server.get(&format!("/status/{token}")).await.0,
            StatusCode::OK
        );
    }
    let payload = signed_session_request(&mut server, RequestDomain::SessionRevoke).await;
    assert_eq!(
        post_json(&server.app, "/api/session/revoke", &payload)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    for token in &tokens {
        assert_eq!(
            server.get(&format!("/status/{token}")).await.0,
            StatusCode::NOT_FOUND
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn session_returns_only_the_signing_credentials_roles_and_handle() {
    let mut server = TestServer::start().await;
    let other = looklab_server::CredentialId::try_from(server.add_credential().await)
        .expect("other credential");
    looklab_server::admin::grant_credential(
        &server.db,
        looklab_trust::Actor::Cli,
        &other,
        looklab_trust::Grant::Admin,
        None,
        "other profile grant",
    )
    .await
    .expect("other grant");
    let mut payload = signed_session_request(&mut server, RequestDomain::Session).await;
    let nonce = URL_SAFE_NO_PAD
        .decode(payload["nonce"].as_str().expect("nonce"))
        .expect("nonce bytes");
    let nonce = Nonce::from_slice(&nonce).expect("nonce size");
    let challenge = Challenge::new(&nonce, &request_digest(RequestDomain::Session, ""));
    payload["pow"] = json!(
        solve(&challenge, 1, &pow_params(), 0)
            .expect("pow")
            .as_array()
    );
    let (status, body) = post_json(&server.app, "/api/session", &payload).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["roles"], json!([]));
    assert_eq!(body["tier"], "new");
    assert_eq!(body["display_handle"], "tester");
    let credential: String =
        sqlx::query_scalar("SELECT id FROM credentials WHERE credential_id = ?")
            .bind(&server.credential_id)
            .fetch_one(&server.db)
            .await
            .expect("credential");
    let credential = looklab_server::CredentialId::try_from(credential).expect("credential id");
    for grant in [
        looklab_trust::Grant::Moderator,
        looklab_trust::Grant::Admin,
        looklab_trust::Grant::Seeder,
    ] {
        looklab_server::admin::grant_credential(
            &server.db,
            looklab_trust::Actor::Cli,
            &credential,
            grant,
            None,
            "session role fixture",
        )
        .await
        .expect("grant");
    }
    sqlx::query("UPDATE credentials SET display_handle = 'helper' WHERE id = ?")
        .bind(credential.as_str())
        .execute(&server.db)
        .await
        .expect("handle");
    let payload = signed_session_request(&mut server, RequestDomain::Session).await;
    let (status, body) = post_json(&server.app, "/api/session", &payload).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["roles"], json!(["moderator", "admin"]));
    assert_eq!(body["tier"], "seeder");
    assert_eq!(body["display_handle"], "helper");
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_out_everywhere_closes_moderator_sessions() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    server
        .set_credential_tier_roles(&server.credential_id, "new", "[\"moderator\"]")
        .await;
    let signed = server.signed_fields("mod-session", &[]).await;
    let (status, session) = post_json(&server.app, "/api/mod/session", &signed).await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let token = session["session_token"].as_str().expect("moderator token");
    let queue_request = || {
        Request::builder()
            .uri("/api/mod/queue")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .expect("queue request")
    };
    assert_eq!(send(&server.app, queue_request()).await.0, StatusCode::OK);
    let signed = signed_session_request(&mut server, RequestDomain::SessionRevoke).await;
    assert_eq!(
        post_json(&server.app, "/api/session/revoke", &signed)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let (status, _, body) = send(&server.app, queue_request()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let error: Value = serde_json::from_slice(&body).expect("error response");
    assert_eq!(error["error"], "TOKEN_UNKNOWN");
}

#[tokio::test(flavor = "multi_thread")]
async fn session_capability_restores_current_own_profile_without_rotating_tokens() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let other = looklab_server::CredentialId::try_from(server.add_credential().await)
        .expect("other credential");
    looklab_server::admin::grant_credential(
        &server.db,
        looklab_trust::Actor::Cli,
        &other,
        looklab_trust::Grant::Admin,
        None,
        "other profile grant",
    )
    .await
    .expect("other grant");
    let signed = signed_session_request(&mut server, RequestDomain::Session).await;
    let (status, session) = post_json(&server.app, "/api/session", &signed).await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let token = session["status_token"].as_str().expect("status token");
    let restore = || {
        Request::builder()
            .uri("/api/session")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .expect("restore request")
    };
    let (status, _, body) = send(&server.app, restore()).await;
    assert_eq!(status, StatusCode::OK);
    let restored: Value = serde_json::from_slice(&body).expect("restored session");
    assert_eq!(restored, session);
    server
        .set_credential_tier_roles(&server.credential_id, "endorsed", "[\"moderator\"]")
        .await;
    let (status, _, body) = send(&server.app, restore()).await;
    assert_eq!(status, StatusCode::OK);
    let restored: Value = serde_json::from_slice(&body).expect("refreshed session");
    assert_eq!(restored["status_token"], token);
    assert_eq!(restored["roles"], json!(["moderator"]));
    assert_eq!(restored["tier"], "endorsed");
    assert_eq!(restored["display_handle"], "tester");
    assert_eq!(server.get("/api/session").await.0, StatusCode::NOT_FOUND);
    let signed = signed_session_request(&mut server, RequestDomain::SessionRevoke).await;
    assert_eq!(
        post_json(&server.app, "/api/session/revoke", &signed)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let (status, _, body) = send(&server.app, restore()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let error: Value = serde_json::from_slice(&body).expect("error response");
    assert_eq!(error["error"], "TOKEN_UNKNOWN");
}

#[tokio::test(flavor = "multi_thread")]
async fn non_session_signed_requests_reject_pow() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    for (domain, path) in [
        ("session-revoke", "/api/session/revoke"),
        ("mod-session", "/api/mod/session"),
    ] {
        let mut signed = server.signed_fields(domain, &[]).await;
        signed["pow"] = json!(vec![0; 16]);
        let (status, error) = post_json(&server.app, path, &signed).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
        assert_eq!(error["error"], "MALFORMED_ENVELOPE");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn session_domains_cannot_authorize_each_other() {
    let mut server = TestServer::start().await;
    assert_eq!(server.upload_valid(None).await.0, StatusCode::CREATED);
    for (domain, endpoint) in [
        (RequestDomain::Session, "/api/session/revoke"),
        (RequestDomain::SessionRevoke, "/api/session"),
    ] {
        let payload = signed_session_request(&mut server, domain).await;
        let (status, body) = post_json(&server.app, endpoint, &payload).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(body["error"], "SIGNATURE_INVALID");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pending_profile_session_requires_valid_pow_and_persists_nothing_on_failure() {
    let mut server = TestServer::start().await;
    for include_invalid_pow in [false, true] {
        let mut payload = signed_session_request(&mut server, RequestDomain::Session).await;
        if include_invalid_pow {
            let nonce = URL_SAFE_NO_PAD
                .decode(payload["nonce"].as_str().expect("nonce"))
                .expect("nonce bytes");
            let nonce = Nonce::from_slice(&nonce).expect("nonce size");
            let challenge = Challenge::new(&nonce, &request_digest(RequestDomain::Session, ""));
            payload["pow"] = json!(bad_pow(&challenge, 1).as_array());
        }
        let (status, body) = post_json(&server.app, "/api/session", &payload).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "POW_INVALID");
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM credentials")
            .fetch_one(&server.db)
            .await
            .expect("credential count");
        assert_eq!(rows, 0);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn audit_log_is_append_only() {
    let server = TestServer::start().await;
    sqlx::query("INSERT INTO audit_log (type, payload, ts) VALUES ('test', '{}', 0)")
        .execute(&server.db)
        .await
        .expect("insert");
    let update = sqlx::query("UPDATE audit_log SET payload = '{}'")
        .execute(&server.db)
        .await;
    assert!(update.is_err(), "audit log accepted an update");
    let delete = sqlx::query("DELETE FROM audit_log")
        .execute(&server.db)
        .await;
    assert!(delete.is_err(), "audit log accepted a delete");
    let replace = sqlx::query(
        "INSERT OR REPLACE INTO audit_log (seq, type, payload, ts) VALUES (1, 'test', '{}', 0)",
    )
    .execute(&server.db)
    .await;
    assert!(replace.is_err(), "audit log accepted a replace");
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_requires_content_length() {
    let server = TestServer::start().await;
    let request = Request::builder()
        .method("POST")
        .uri("/api/upload")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::empty())
        .expect("request");
    let (status, _, body) = send(&server.app, request).await;
    assert_eq!(status, StatusCode::LENGTH_REQUIRED);
    let value: Value = serde_json::from_slice(&body).expect("error json");
    assert_eq!(value["error"], "LENGTH_REQUIRED");
}

const SENTINEL: &str = "203.0.113.77";

#[tokio::test(flavor = "multi_thread")]
async fn nothing_sensitive_reaches_disk_logs_or_responses() {
    let mut server = TestServer::start().await;
    let addr = server.listen().await;
    let serial = RawSerial::new(SERIAL.as_bytes()).expect("serial");
    let mut responses: Vec<Vec<u8>> = Vec::new();

    let (status, body) = tcp_nonce(addr).await;
    assert_eq!(status, 201, "nonce: {body:?}");
    responses.push(body.clone());
    let nonce = nonce_from_json(&body);

    let (manifest, blobs) = manifest(valid_settings());
    let pow_challenge = Challenge::new(&nonce, &manifest_hash(&manifest));
    let challenge = send_challenge(&nonce, &manifest);
    let assertion = server.assertion(&challenge).await;
    let pow = solve(&pow_challenge, 2, &pow_params(), 0).expect("pow");
    let bytes = envelope(nonce, &manifest, &blobs, Some(&serial), &assertion, &pow);
    let (status, body) = tcp_request(
        addr,
        "POST",
        "/api/upload",
        Some("application/octet-stream"),
        &bytes,
    )
    .await;
    assert_eq!(status, 201, "upload: {body:?}");
    responses.push(body.clone());
    let value: Value = serde_json::from_slice(&body).expect("upload json");
    let token = value["status_token"].as_str().expect("token").to_owned();

    let (status, body) = tcp_request(addr, "GET", &format!("/status/{token}"), None, &[]).await;
    assert_eq!(status, 200);
    responses.push(body);
    let (status, body) = tcp_request(addr, "GET", &format!("/status/{token}.rss"), None, &[]).await;
    assert_eq!(status, 200);
    responses.push(body);

    let (status, body) = tcp_nonce(addr).await;
    assert_eq!(status, 201);
    responses.push(body.clone());
    let nonce = nonce_from_json(&body);
    let pow_challenge = Challenge::new(&nonce, &manifest_hash(&manifest));
    let challenge = send_challenge(&nonce, &manifest);
    let mut assertion = server.assertion(&challenge).await;
    assertion.signature[0] ^= 0xff;
    let pow = solve(&pow_challenge, 2, &pow_params(), 0).expect("pow");
    let (status, body) = tcp_request(
        addr,
        "POST",
        "/api/upload",
        Some("application/octet-stream"),
        &envelope(nonce, &manifest, &blobs, None, &assertion, &pow),
    )
    .await;
    assert_eq!(status, 401);
    responses.push(body);

    let (status, body) = tcp_nonce(addr).await;
    assert_eq!(status, 201);
    responses.push(body.clone());
    let nonce = nonce_from_json(&body);
    let pow_challenge = Challenge::new(&nonce, &manifest_hash(&manifest));
    let challenge = send_challenge(&nonce, &manifest);
    let assertion = server.assertion(&challenge).await;
    let pow = bad_pow(&pow_challenge, 2);
    let (status, body) = tcp_request(
        addr,
        "POST",
        "/api/upload",
        Some("application/octet-stream"),
        &envelope(nonce, &manifest, &blobs, None, &assertion, &pow),
    )
    .await;
    assert_eq!(status, 400);
    responses.push(body);

    let hmac = serial.hmac(&SerialHmacKey::new([9; 32]), "Fujifilm", "X-T5");
    let hmac_bytes = hmac.as_bytes().to_vec();

    let credential_json: String = sqlx::query_scalar("SELECT webauthn_credential FROM credentials")
        .fetch_one(&server.db)
        .await
        .expect("credential");
    let passkey: webauthn_rs::prelude::Passkey =
        serde_json::from_str(&credential_json).expect("stored passkey");
    let mut key_needles = public_key_needles(&passkey);
    let mut key_file_needles = Vec::new();
    if let webauthn_rs::prelude::COSEKeyType::EC_EC2(ec) = &passkey.get_public_key().key {
        let mut xy = ec.x.clone();
        xy.extend_from_slice(&ec.y);
        key_file_needles.push(ec.x.clone());
        key_file_needles.push(ec.y.clone());
        key_file_needles.push(xy);
        let debug = vec![
            format!("{ec:?}").into_bytes(),
            format!("{:?}", ec.x).into_bytes(),
            format!("{:?}", ec.y).into_bytes(),
        ];
        key_file_needles.extend(debug.clone());
        key_needles.extend(debug);
    }
    assert!(
        key_needles
            .iter()
            .any(|needle| contains(credential_json.as_bytes(), needle)),
        "the stored credential does not contain the public key encoding"
    );

    let stored_serial_hmac: Vec<u8> =
        sqlx::query_scalar("SELECT serial_hmac FROM contributions LIMIT 1")
            .fetch_one(&server.db)
            .await
            .expect("serial hmac");
    assert_eq!(stored_serial_hmac, hmac_bytes);

    let serial_needles = encoded_needles(SERIAL.as_bytes());
    let hmac_needles = encoded_needles(&hmac_bytes);
    let sentinel_needles = vec![SENTINEL.as_bytes().to_vec(), b"127.0.0.1".to_vec()];

    let log_bytes = log_bytes();
    assert!(!log_bytes.is_empty(), "no log output was captured");
    assert_absent("logs", &log_bytes, &serial_needles);
    assert_absent("logs", &log_bytes, &hmac_needles);
    assert_absent("logs", &log_bytes, &key_needles);
    assert_absent("logs", &log_bytes, &sentinel_needles);
    for address in [
        "::1",
        "2001:db8::1",
        "2001:db8:0:0:0:0:0:1",
        "::ffff:192.0.2.1",
    ] {
        assert!(contains_ipv6(&format!("peer=[{address}]:443")));
    }
    assert!(!contains_ipv6("looklab_server::error: request failed"));
    for run in text_runs(&log_bytes) {
        assert!(!contains_ipv4(&run), "IPv4 address in logs: {run}");
        assert!(!contains_ipv6(&run), "IPv6 address in logs: {run}");
    }

    for response in &responses {
        assert_absent("responses", response, &serial_needles);
        assert_absent("responses", response, &hmac_needles);
        assert_absent("responses", response, &key_needles);
        assert_absent("responses", response, &sentinel_needles);
    }

    let db_files = database_files(server.dir.path());
    assert!(!db_files.is_empty());
    for file in &db_files {
        let bytes = std::fs::read(file).expect("read database file");
        assert_absent("database", &bytes, &serial_needles);
        assert_absent("database", &bytes, &key_file_needles);
        assert_absent("database", &bytes, &sentinel_needles);
    }
    for run in text_runs(&database_bytes(&db_files)) {
        assert!(!contains_ipv4(&run), "IPv4 address in database: {run}");
        assert!(!contains_ipv6(&run), "IPv6 address in database: {run}");
    }

    for entry in std::fs::read_dir(&server.blob_dir).expect("blob dir") {
        let path = entry.expect("entry").path();
        let bytes = std::fs::read(&path).expect("blob");
        assert_absent("blobs", &bytes, &serial_needles);
        assert_absent("blobs", &bytes, &hmac_needles);
        assert_absent("blobs", &bytes, &key_needles);
        assert_absent("blobs", &bytes, &sentinel_needles);
        for run in text_runs(&bytes) {
            assert!(!contains_ipv4(&run), "IPv4 address in blob: {run}");
            assert!(!contains_ipv6(&run), "IPv6 address in blob: {run}");
        }
    }

    for (table, column, value) in column_values(&server.db).await {
        if table == "credentials" && column == "webauthn_credential" {
            continue;
        }
        assert_absent(&format!("{table}.{column}"), &value, &key_needles);
        assert_absent(&format!("{table}.{column}"), &value, &serial_needles);
        assert_absent(&format!("{table}.{column}"), &value, &sentinel_needles);
    }
}

fn nonce_from_json(body: &[u8]) -> Nonce {
    let value: Value = serde_json::from_slice(body).expect("nonce json");
    let bytes = URL_SAFE_NO_PAD
        .decode(value["nonce"].as_str().expect("nonce string"))
        .expect("nonce base64");
    Nonce::from_slice(&bytes).expect("nonce length")
}

fn encoded_needles(value: &[u8]) -> Vec<Vec<u8>> {
    vec![
        value.to_vec(),
        hex(value).into_bytes(),
        hex(value).to_uppercase().into_bytes(),
        STANDARD.encode(value).into_bytes(),
        URL_SAFE_NO_PAD.encode(value).into_bytes(),
        format!("{value:?}").into_bytes(),
    ]
}

fn assert_absent(context: &str, haystack: &[u8], needles: &[Vec<u8>]) {
    for needle in needles {
        assert!(
            !contains(haystack, needle),
            "{context} contains {:?}",
            String::from_utf8_lossy(needle)
        );
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn public_key_needles(passkey: &webauthn_rs::prelude::Passkey) -> Vec<Vec<u8>> {
    let webauthn_rs::prelude::COSEKeyType::EC_EC2(ec) = &passkey.get_public_key().key else {
        panic!("the software authenticator uses an EC2 key");
    };
    let mut needles = vec![ec.x.clone(), ec.y.clone()];
    let mut xy = ec.x.clone();
    xy.extend_from_slice(&ec.y);
    needles.push(xy);
    for part in [&ec.x, &ec.y] {
        needles.push(URL_SAFE_NO_PAD.encode(part).into_bytes());
        needles.push(STANDARD.encode(part).into_bytes());
        needles.push(hex(part).into_bytes());
        needles.push(hex(part).to_uppercase().into_bytes());
    }
    needles
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.len() <= haystack.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn database_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).expect("data dir") {
        let path = entry.expect("entry").path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with("looklab.sqlite3") {
            files.push(path);
        }
    }
    files
}

fn database_bytes(files: &[PathBuf]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for file in files {
        bytes.extend(std::fs::read(file).expect("database file"));
    }
    bytes
}

fn text_runs(bytes: &[u8]) -> Vec<String> {
    let mut runs = Vec::new();
    let mut current = Vec::new();
    for &byte in bytes {
        let printable =
            byte == b'\n' || byte == b'\r' || byte == b'\t' || (0x20..=0x7e).contains(&byte);
        if printable {
            current.push(byte);
        } else {
            if current.len() >= 7 {
                runs.push(String::from_utf8_lossy(&current).into_owned());
            }
            current.clear();
        }
    }
    if current.len() >= 7 {
        runs.push(String::from_utf8_lossy(&current).into_owned());
    }
    runs
}

fn contains_ipv4(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let mut cursor = index;
        let mut parts = 0;
        let mut valid = true;
        while parts < 4 {
            let start = cursor;
            while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                cursor += 1;
            }
            if cursor == start || cursor - start > 3 {
                valid = false;
                break;
            }
            let value: u32 = text[start..cursor].parse().expect("digits");
            if value > 255 {
                valid = false;
                break;
            }
            parts += 1;
            if parts < 4 {
                if cursor < bytes.len() && bytes[cursor] == b'.' {
                    cursor += 1;
                } else {
                    valid = false;
                    break;
                }
            }
        }
        if valid && parts == 4 {
            let before_ok = index == 0 || !bytes[index - 1].is_ascii_digit();
            let after_ok =
                cursor == bytes.len() || !(bytes[cursor].is_ascii_digit() || bytes[cursor] == b'.');
            if before_ok && after_ok {
                return true;
            }
        }
        index += 1;
    }
    false
}

fn contains_ipv6(text: &str) -> bool {
    text.split(|character: char| {
        !character.is_ascii_alphanumeric() && !matches!(character, ':' | '.' | '_')
    })
    .any(|token| {
        token
            .trim_end_matches('.')
            .parse::<std::net::Ipv6Addr>()
            .is_ok()
    })
}

async fn column_values(db: &SqlitePool) -> Vec<(String, String, Vec<u8>)> {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(db)
    .await
    .expect("tables");
    let mut values = Vec::new();
    for table in tables {
        let columns: Vec<(i64, String)> = sqlx::query_as(&format!("PRAGMA table_info({table})"))
            .fetch_all(db)
            .await
            .expect("columns")
            .into_iter()
            .map(|row: (i64, String, String, i64, Option<String>, i64)| (row.0, row.1))
            .collect();
        let rows = sqlx::query(&format!("SELECT * FROM {table}"))
            .fetch_all(db)
            .await
            .expect("rows");
        for row in rows {
            for (index, name) in &columns {
                let position = usize::try_from(*index).expect("column index");
                if let Ok(text) = row.try_get::<String, _>(position) {
                    values.push((table.clone(), name.clone(), text.into_bytes()));
                } else if let Ok(bytes) = row.try_get::<Vec<u8>, _>(position) {
                    values.push((table.clone(), name.clone(), bytes));
                }
            }
        }
    }
    values
}

#[tokio::test(flavor = "multi_thread")]
async fn grants_endorsements_and_root_admissions_are_logged_and_published() {
    use looklab_server::RootAdmissionId;
    use looklab_server::admin::{ManifestArtifact, write_version_manifest};

    let mut server = TestServer::start().await;
    let serial = RawSerial::new(SERIAL.as_bytes()).expect("serial");
    let (status, body) = server.upload_valid(Some(&serial)).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
    let contribution_id: String = sqlx::query_scalar("SELECT id FROM contributions LIMIT 1")
        .fetch_one(&server.db)
        .await
        .expect("contribution");
    sqlx::query("UPDATE contributions SET kind = 'chart' WHERE id = ?")
        .bind(&contribution_id)
        .execute(&server.db)
        .await
        .expect("chart contribution");
    let second = server.add_credential().await;
    let endorsed = server.add_credential().await;
    let actor = server.credential_uuid().await;
    server
        .set_credential_tier_roles(&server.credential_id, "seeder", "[\"admin\"]")
        .await;

    let digest = bind_fields(&[second.as_bytes(), b"seeder", b"", b"bootstrap"]);
    let (status, grant) = server
        .post_signed(
            "/api/admin/grant",
            "grant",
            &digest,
            json!({ "subject": second, "grant": "seeder", "scope": Value::Null, "reason": "bootstrap" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "grant: {grant}");
    assert_eq!(grant["subject"], second);
    assert_eq!(grant["grant"], "seeder");
    assert!(grant["revoked_at"].is_null());
    let grants: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grants")
        .fetch_one(&server.db)
        .await
        .expect("grants");
    assert_eq!(grants, 1);
    let last: String = sqlx::query_scalar("SELECT type FROM audit_log ORDER BY seq DESC LIMIT 1")
        .fetch_one(&server.db)
        .await
        .expect("audit type");
    assert_eq!(last, "grant_created");
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT tier FROM credentials WHERE id = ?")
            .bind(&second)
            .fetch_one(&server.db)
            .await
            .expect("tier"),
        "seeder"
    );

    let scope = json!({ "rules": [{ "body": "fujifilm/x-t5", "look": Value::Null }] });
    sqlx::query("UPDATE contributions SET render_class = 'monochrome' WHERE id = ?")
        .bind(&contribution_id)
        .execute(&server.db)
        .await
        .expect("deferred contribution");
    let image_id: String =
        sqlx::query_scalar("SELECT id FROM contribution_images WHERE contribution_id = ? LIMIT 1")
            .bind(&contribution_id)
            .fetch_one(&server.db)
            .await
            .expect("image");
    let deferred_scope_bytes = serde_json::to_vec(&scope).expect("scope");
    for reference in [
        format!("contribution:{contribution_id}"),
        format!("image:{image_id}"),
        "contribution:not-a-uuid".to_owned(),
        format!("contribution:{}", uuid::Uuid::new_v4()),
    ] {
        let deferred_evidence = vec![reference];
        let deferred_bytes = serde_json::to_vec(&deferred_evidence).expect("deferred evidence");
        let digest = bind_fields(&[
            endorsed.as_bytes(),
            &deferred_scope_bytes,
            b"reviewed chart",
            &deferred_bytes,
        ]);
        let (status, body) = server
            .post_signed(
                "/api/seeder/endorse",
                "endorse",
                &digest,
                json!({
                    "subject": endorsed,
                    "scope": scope.clone(),
                    "reason": "reviewed chart",
                    "evidence": deferred_evidence,
                }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "deferred endorse: {body}");
        assert_eq!(body["error"], "MALFORMED_ENVELOPE");
    }
    sqlx::query("UPDATE contributions SET render_class = 'colour' WHERE id = ?")
        .bind(&contribution_id)
        .execute(&server.db)
        .await
        .expect("colour contribution");

    let evidence = vec!["chart-session-1"];
    let scope_bytes = serde_json::to_vec(&scope).expect("scope");
    let evidence_bytes = serde_json::to_vec(&evidence).expect("evidence");
    let digest = bind_fields(&[
        endorsed.as_bytes(),
        &scope_bytes,
        b"reviewed chart",
        &evidence_bytes,
    ]);
    let (status, endorsement) = server
        .post_signed(
            "/api/seeder/endorse",
            "endorse",
            &digest,
            json!({
                "subject": endorsed,
                "scope": scope,
                "reason": "reviewed chart",
                "evidence": evidence,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "endorse: {endorsement}");
    assert_eq!(endorsement["scope_text"], "Fujifilm X-T5 · all looks");
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT tier FROM credentials WHERE id = ?")
            .bind(&endorsed)
            .fetch_one(&server.db)
            .await
            .expect("tier"),
        "endorsed"
    );

    let root_scope = json!({ "rules": [{ "body": "fujifilm/x-t5", "look": "classic-chrome" }] });
    let root_scope_bytes = serde_json::to_vec(&root_scope).expect("scope");
    let digest = bind_fields(&[
        contribution_id.as_bytes(),
        &root_scope_bytes,
        b"anchored session",
    ]);
    let (status, root) = server
        .post_signed(
            "/api/seeder/admit-root",
            "admit-root",
            &digest,
            json!({
                "contribution_id": contribution_id,
                "scope": root_scope,
                "reason": "anchored session",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "admit: {root}");
    assert_eq!(root["subject"], actor);
    let root_id = root["id"].as_str().expect("root id").to_owned();

    let look = looklab_upload::LookSlug::try_from("classic-chrome").expect("look");
    let artifacts = vec![ManifestArtifact {
        name: "look.json".to_owned(),
        sha256: "ab".repeat(32),
    }];
    let root_typed = RootAdmissionId::try_from(root_id.clone()).expect("root id");
    let manifest = write_version_manifest(
        &server.db,
        &look,
        looklab_trust::VersionId::new(1),
        looklab_server::admin::VersionStatus::Published,
        &artifacts,
        std::slice::from_ref(&root_typed),
    )
    .await
    .expect("manifest");
    assert_eq!(
        manifest.status,
        looklab_server::admin::VersionStatus::Published
    );
    let (status, _, body) = server.get("/manifests/classic-chrome/1.json").await;
    assert_eq!(status, StatusCode::OK, "manifest: {body:?}");
    let served: Value = serde_json::from_slice(&body).expect("manifest json");
    assert_eq!(served["artifacts"][0]["name"], "look.json");
    assert_eq!(served["artifacts"][0]["sha256"], "ab".repeat(32));

    let digest = bind_fields(&[root_id.as_bytes(), b"root withdrawn"]);
    let (status, revocation) = server
        .post_signed(
            "/api/seeder/revoke-root",
            "revoke-root",
            &digest,
            json!({ "root_admission_id": root_id, "reason": "root withdrawn" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "revoke root: {revocation}");
    assert_eq!(revocation["new_versions"][0]["look"], "classic-chrome");
    assert_eq!(revocation["new_versions"][0]["version"], 2);
    assert_eq!(
        revocation["regating"].as_array().expect("regating").len(),
        1
    );
    let versions: Vec<i64> = sqlx::query_scalar(
        "SELECT version FROM look_versions WHERE look = 'classic-chrome' ORDER BY version",
    )
    .fetch_all(&server.db)
    .await
    .expect("versions");
    assert_eq!(versions, vec![1, 2]);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM look_versions WHERE version = 2")
            .fetch_one(&server.db)
            .await
            .expect("status"),
        "provisional"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM version_regating")
            .fetch_one(&server.db)
            .await
            .expect("regating"),
        1
    );

    let digest = bind_fields(&[endorsed.as_bytes(), b"evidence withdrawn"]);
    let (status, revoked) = server
        .post_signed(
            "/api/seeder/revoke-endorsement",
            "revoke-endorsement",
            &digest,
            json!({ "subject": endorsed, "reason": "evidence withdrawn" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "revoke endorsement: {revoked}");
    assert_eq!(revoked["revoked"], 1);

    let digest = bind_fields(&[second.as_bytes(), b"seeder", b"role withdrawn"]);
    let (status, revoked) = server
        .post_signed(
            "/api/admin/revoke-grant",
            "revoke-grant",
            &digest,
            json!({ "subject": second, "grant": "seeder", "reason": "role withdrawn" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "revoke grant: {revoked}");
    assert!(revoked["revoked_at"].is_number());

    let (status, _, body) = server.get("/lists/grants.json").await;
    assert_eq!(status, StatusCode::OK);
    let grants: Value = serde_json::from_slice(&body).expect("grants json");
    assert_eq!(grants["grants"].as_array().expect("grants").len(), 1);
    assert!(grants["grants"][0]["revoked_at"].is_number());
    let (status, _, body) = server.get("/lists/endorsements.json").await;
    assert_eq!(status, StatusCode::OK);
    let endorsements: Value = serde_json::from_slice(&body).expect("endorsements json");
    assert_eq!(
        endorsements["endorsements"]
            .as_array()
            .expect("endorsements")
            .len(),
        1
    );
    assert!(endorsements["endorsements"][0]["revoked_at"].is_number());

    let (status, _, body) = server.get("/log.json?limit=100").await;
    assert_eq!(status, StatusCode::OK);
    let log: Value = serde_json::from_slice(&body).expect("log json");
    let kinds: std::collections::HashSet<&str> = log["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .filter_map(|entry| entry["type"].as_str())
        .collect();
    for expected in [
        "grant_created",
        "endorsement_issued",
        "root_admitted",
        "release_manifest",
        "root_revoked",
        "endorsement_revoked",
        "grant_revoked",
    ] {
        assert!(kinds.contains(expected), "missing audit entry {expected}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn moderator_session_serves_the_queue_and_records_decisions() {
    let mut server = TestServer::start().await;
    let second = server.add_credential().await;
    let actor = server.persist_credential().await;
    server
        .set_credential_tier_roles(&server.credential_id, "seeder", "[\"moderator\"]")
        .await;
    sqlx::query("UPDATE credentials SET reputation = 1.4 WHERE id = ?")
        .bind(&second)
        .execute(&server.db)
        .await
        .expect("reputation");

    let proposal = looklab_server::ProposalId::new();
    sqlx::query(
        "INSERT INTO proposals (id, credential_id, type, payload, human_summary, evidence, status, created_at)
         VALUES (?, ?, 'body', ?, 'adds a body', '[]', 'pending', 0)",
    )
    .bind(proposal.as_str())
    .bind(&actor)
    .bind(include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml"))
    .execute(&server.db)
    .await
    .expect("proposal");
    sqlx::query(
        "INSERT INTO gate_failures (id, look, version, report, status, created_at)
         VALUES ('gate-1', 'classic-chrome', 1, '{\"median\":0.4}', 'pending', 0)",
    )
    .execute(&server.db)
    .await
    .expect("gate failure");
    for credential in [&actor, &second] {
        let id = looklab_server::ContributionId::new();
        sqlx::query(
            "INSERT INTO contributions
             (id, credential_id, batch_manifest_hash, signature, pow, kind, serial_hmac, intent,
              body_id, firmware_version, sim_key, settings, status, created_at)
             VALUES (?, ?, x'00', x'00', 'pow', 'chart',
                     x'0101010101010101010101010101010101010101010101010101010101010101',
                     'native', 'fujifilm/x-t5', '2.10', 'classic-chrome', '{}', 'pending', 0)",
        )
        .bind(id.as_str())
        .bind(credential)
        .execute(&server.db)
        .await
        .expect("contribution");
    }

    let signed = server.signed_fields("mod-session", &[]).await;
    let (status, session) = post_json(&server.app, "/api/mod/session", &signed).await;
    assert_eq!(status, StatusCode::OK, "session: {session}");
    let token = session["session_token"].as_str().expect("token").to_owned();
    assert_eq!(session["expires_in_seconds"], 900);

    let request = Request::builder()
        .method("GET")
        .uri("/api/mod/queue")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request");
    let (status, _, body) = send(&server.app, request).await;
    assert_eq!(status, StatusCode::OK, "queue: {body:?}");
    let queue: Value = serde_json::from_slice(&body).expect("queue json");
    assert_eq!(queue["proposals"].as_array().expect("proposals").len(), 1);
    assert_eq!(queue["proposals"][0]["kind"], "body");
    assert_eq!(queue["failed_gates"].as_array().expect("gates").len(), 1);
    assert_eq!(queue["serial_reuse"].as_array().expect("reuse").len(), 1);
    assert_eq!(
        queue["serial_reuse"][0]["contributions"]
            .as_array()
            .expect("reuse contributions")
            .len(),
        2
    );
    assert!(
        queue["nominations"]
            .as_array()
            .expect("nominations")
            .iter()
            .any(|nomination| nomination["credential"] == second),
        "second credential is nominated"
    );

    let decision = json!({
        "proposal_id": proposal.as_str(),
        "decision": "approve",
        "reason": "validated locally",
    });
    let request = Request::builder()
        .method("POST")
        .uri("/api/mod/decision")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&decision).expect("decision")))
        .expect("request");
    let (status, _, body) = send(&server.app, request).await;
    assert_eq!(status, StatusCode::OK, "decision: {body:?}");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM moderation_decisions")
            .fetch_one(&server.db)
            .await
            .expect("decisions"),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM proposals WHERE id = ?")
            .bind(proposal.as_str())
            .fetch_one(&server.db)
            .await
            .expect("proposal status"),
        "approved"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT type FROM audit_log ORDER BY seq DESC LIMIT 1")
            .fetch_one(&server.db)
            .await
            .expect("audit type"),
        "moderation_decision"
    );

    let request = Request::builder()
        .method("GET")
        .uri("/api/mod/queue")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request");
    let (status, _, body) = send(&server.app, request).await;
    assert_eq!(status, StatusCode::OK);
    let queue: Value = serde_json::from_slice(&body).expect("queue json");
    assert!(queue["proposals"].as_array().expect("proposals").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn moderator_revert_route_republishes_and_requests_a_refit() {
    use looklab_model::file::{LookIdentity, LookKind, WbPolicy};
    use looklab_model::{Look, LookDocument};
    use looklab_server::admin::{self, ManifestArtifact};

    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let token = server.moderator_token().await;
    let look = looklab_upload::LookSlug::try_from("classic-chrome".to_owned()).expect("look");
    let identity = LookIdentity::new(
        "classic-chrome",
        "1.0.0",
        LookKind::NativeFamily,
        WbPolicy::Adaptive,
    )
    .expect("identity");
    let document = LookDocument::from_look(identity, Look::colorimetric()).expect("document");
    let look_bytes = document.encode().expect("look bytes");
    let look_hash = hex(&Sha256::digest(&look_bytes));
    std::fs::write(server.blob_dir.join(&look_hash), &look_bytes).expect("look blob");
    let release = json!({
        "version": 1,
        "gauge": 0.0,
        "anchor": 1,
        "clip_k": 1.0,
        "probes": [],
        "gated_since_anchor": {"probes": []},
        "converged": true,
        "gate": {},
        "provenance": {"anchor_version": "1.0.0"},
        "created_at": 0,
    });
    let release_bytes = serde_json::to_vec(&release).expect("release");
    let release_hash = hex(&Sha256::digest(&release_bytes));
    std::fs::write(server.blob_dir.join(&release_hash), &release_bytes).expect("release blob");
    let artifacts = vec![
        ManifestArtifact {
            name: looklab_publish::DOCUMENT_ARTIFACT.to_owned(),
            sha256: look_hash,
        },
        ManifestArtifact {
            name: "release.json".to_owned(),
            sha256: release_hash,
        },
    ];
    admin::write_version_manifest(
        &server.db,
        &look,
        looklab_trust::VersionId::new(1),
        admin::VersionStatus::Published,
        &artifacts,
        &[],
    )
    .await
    .expect("version 1");
    sqlx::query(
        "INSERT INTO model_subjects (key, kind, family, created_at)
         VALUES ('shared_family:classic-chrome', 'shared_family', 'classic-chrome', 0)
         ON CONFLICT(key) DO NOTHING",
    )
    .execute(&server.db)
    .await
    .expect("subject");
    sqlx::query(
        "INSERT INTO model_subject_revisions (subject_key, revision, role, recorded_at)
         VALUES ('shared_family:classic-chrome', 1, 'target', 0)
         ON CONFLICT(subject_key, revision) DO NOTHING",
    )
    .execute(&server.db)
    .await
    .expect("accepted marker");

    let (status, reply) = server
        .revert_look(
            &token,
            json!({
                "look": "classic-chrome",
                "target_version": 1,
                "rejected_contributions": [],
                "reason": "restore the released fit",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "revert: {reply}");
    assert_eq!(reply["target_version"], 1);
    assert_eq!(reply["new_version"], 2);
    let manifests: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM version_manifests m
         JOIN look_versions v ON v.id = m.look_version_id
         WHERE v.look = 'classic-chrome' AND v.version = 2",
    )
    .fetch_one(&server.db)
    .await
    .expect("new manifest");
    assert_eq!(manifests, 1);

    let audit: String = sqlx::query_scalar("SELECT type FROM audit_log ORDER BY seq DESC LIMIT 1")
        .fetch_one(&server.db)
        .await
        .expect("audit");
    assert_eq!(audit, "revert");
    let queued: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE kind = 'revert_refit' AND status = 'queued'",
    )
    .fetch_one(&server.db)
    .await
    .expect("refit job");
    assert_eq!(queued, 1);
    let requested: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM look_refit_requests WHERE look = 'classic-chrome'",
    )
    .fetch_one(&server.db)
    .await
    .expect("refit request");
    assert_eq!(requested, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_and_public_endpoints_publish_no_secrets() {
    use looklab_server::RootAdmissionId;
    use looklab_server::admin::{ManifestArtifact, write_version_manifest};

    let mut server = TestServer::start().await;
    let serial = RawSerial::new(SERIAL.as_bytes()).expect("serial");
    let (status, body) = server.upload_valid(Some(&serial)).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
    let hmac = serial.hmac(&SerialHmacKey::new([9; 32]), "Fujifilm", "X-T5");
    let contribution_id: String = sqlx::query_scalar("SELECT id FROM contributions LIMIT 1")
        .fetch_one(&server.db)
        .await
        .expect("contribution");
    sqlx::query("UPDATE contributions SET kind = 'chart' WHERE id = ?")
        .bind(&contribution_id)
        .execute(&server.db)
        .await
        .expect("chart contribution");
    let second = server.add_credential().await;
    let endorsed = server.add_credential().await;
    let actor = server.credential_uuid().await;
    server
        .set_credential_tier_roles(&server.credential_id, "seeder", "[\"admin\",\"moderator\"]")
        .await;

    let mut responses: Vec<Vec<u8>> = Vec::new();
    let digest = bind_fields(&[second.as_bytes(), b"seeder", b"", b"bootstrap"]);
    let (status, grant) = server
        .post_signed(
            "/api/admin/grant",
            "grant",
            &digest,
            json!({ "subject": second, "grant": "seeder", "scope": Value::Null, "reason": "bootstrap" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "grant: {grant}");
    responses.push(serde_json::to_vec(&grant).expect("grant json"));

    let scope = json!({ "rules": [{ "body": "fujifilm/x-t5", "look": Value::Null }] });
    let scope_bytes = serde_json::to_vec(&scope).expect("scope");
    let digest = bind_fields(&[endorsed.as_bytes(), &scope_bytes, b"reviewed", b"[]"]);
    let (status, endorsement) = server
        .post_signed(
            "/api/seeder/endorse",
            "endorse",
            &digest,
            json!({
                "subject": endorsed,
                "scope": scope,
                "reason": "reviewed",
                "evidence": [],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "endorse: {endorsement}");
    responses.push(serde_json::to_vec(&endorsement).expect("endorsement json"));

    let root_scope = json!({ "rules": [{ "body": "fujifilm/x-t5", "look": "classic-chrome" }] });
    let root_scope_bytes = serde_json::to_vec(&root_scope).expect("scope");
    let digest = bind_fields(&[contribution_id.as_bytes(), &root_scope_bytes, b"anchored"]);
    let (status, root) = server
        .post_signed(
            "/api/seeder/admit-root",
            "admit-root",
            &digest,
            json!({
                "contribution_id": contribution_id,
                "scope": root_scope,
                "reason": "anchored",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "admit: {root}");
    responses.push(serde_json::to_vec(&root).expect("root json"));
    let root_id = root["id"].as_str().expect("root id").to_owned();

    let look = looklab_upload::LookSlug::try_from("classic-chrome").expect("look");
    write_version_manifest(
        &server.db,
        &look,
        looklab_trust::VersionId::new(1),
        looklab_server::admin::VersionStatus::Published,
        &[ManifestArtifact {
            name: "look.json".to_owned(),
            sha256: "cd".repeat(32),
        }],
        &[RootAdmissionId::try_from(root_id).expect("root id")],
    )
    .await
    .expect("manifest");

    let proposal = looklab_server::ProposalId::new();
    sqlx::query(
        "INSERT INTO proposals (id, credential_id, type, payload, human_summary, evidence, status, created_at)
         VALUES (?, ?, 'body', ?, 'adds a body', '[]', 'pending', 0)",
    )
    .bind(proposal.as_str())
    .bind(&actor)
    .bind(include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml"))
    .execute(&server.db)
    .await
    .expect("proposal");

    let signed = server.signed_fields("mod-session", &[]).await;
    let (status, session) = post_json(&server.app, "/api/mod/session", &signed).await;
    assert_eq!(status, StatusCode::OK, "session: {session}");
    responses.push(serde_json::to_vec(&session).expect("session json"));
    let token = session["session_token"].as_str().expect("token").to_owned();
    let request = Request::builder()
        .method("GET")
        .uri("/api/mod/queue")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request");
    let (status, _, body) = send(&server.app, request).await;
    assert_eq!(status, StatusCode::OK, "queue: {body:?}");
    responses.push(body.to_vec());

    for path in [
        "/lists/grants.json",
        "/lists/endorsements.json",
        "/log.json?limit=100",
        "/manifests/classic-chrome/1.json",
    ] {
        let (status, _, body) = server.get(path).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body:?}");
        responses.push(body.to_vec());
    }

    let mut key_needles = Vec::new();
    let credentials: Vec<String> =
        sqlx::query_scalar("SELECT webauthn_credential FROM credentials")
            .fetch_all(&server.db)
            .await
            .expect("credentials");
    for json in credentials {
        if let Ok(passkey) = serde_json::from_str::<webauthn_rs::prelude::Passkey>(&json) {
            key_needles.extend(public_key_needles(&passkey));
        }
    }
    let serial_needles = encoded_needles(SERIAL.as_bytes());
    let hmac_needles = encoded_needles(hmac.as_bytes());
    for response in &responses {
        assert_absent("responses", response, &key_needles);
        assert_absent("responses", response, &serial_needles);
        assert_absent("responses", response, &hmac_needles);
    }
    let log_bytes = log_bytes();
    assert_absent("logs", &log_bytes, &key_needles);
    assert_absent("logs", &log_bytes, &serial_needles);
    assert_absent("logs", &log_bytes, &hmac_needles);
    for (table, column, value) in column_values(&server.db).await {
        if table == "credentials" && column == "webauthn_credential" {
            continue;
        }
        assert_absent(&format!("{table}.{column}"), &value, &key_needles);
        assert_absent(&format!("{table}.{column}"), &value, &serial_needles);
    }
}

#[derive(serde::Serialize)]
struct ProposalSigningBody<'a> {
    kind: &'a str,
    payload: &'a str,
    fixtures: &'a [FixtureDonation],
}

#[derive(serde::Serialize)]
struct ConfirmationSigningBody<'a> {
    proposal_id: &'a str,
    tags: &'a [LabeledTag],
}

#[derive(serde::Serialize)]
struct ThreadSigningBody<'a> {
    proposal_id: &'a str,
}

#[derive(serde::Serialize)]
struct MessageSigningBody<'a> {
    proposal_id: &'a str,
    body: &'a str,
}

impl TestServer {
    async fn signed_proposal_body(
        &mut self,
        kind: ProposalKind,
        payload: &str,
        fixtures: &[FixtureDonation],
        pow_ok: bool,
    ) -> Value {
        let kind_text = kind.to_string();
        let signing = ProposalSigningBody {
            kind: &kind_text,
            payload,
            fixtures,
        };
        let bytes = serde_json::to_vec(&signing).expect("signing body");
        let nonce = self.nonce().await;
        let digest = action_digest("proposal", &bytes);
        let challenge = Challenge::new(&nonce, &digest);
        let assertion = self.assertion(&challenge).await;
        let pow = if pow_ok {
            solve(&challenge, 1, &pow_params(), 0).expect("proof-of-work")
        } else {
            bad_pow(&challenge, 1)
        };
        json!({
            "kind": kind,
            "payload": payload,
            "fixtures": serde_json::to_value(fixtures).expect("fixtures json"),
            "pow": pow.as_array().to_vec(),
            "nonce": b64(nonce.as_bytes()),
            "credential_id": b64(&assertion.credential_id),
            "authenticator_data": b64(&assertion.authenticator_data),
            "client_data_json": b64(&assertion.client_data_json),
            "signature": b64(&assertion.signature),
        })
    }

    async fn submit_proposal(
        &mut self,
        kind: &str,
        payload: &str,
        fixtures: &[FixtureDonation],
        pow_ok: bool,
    ) -> (StatusCode, Value) {
        let kind = kind.parse::<ProposalKind>().expect("proposal kind");
        let body = self
            .signed_proposal_body(kind, payload, fixtures, pow_ok)
            .await;
        post_json(&self.app, "/api/proposals", &body).await
    }

    async fn submit_lut(&mut self, kind: &str, payload: &str, bytes: &[u8]) -> (StatusCode, Value) {
        let fixtures: &[FixtureDonation] = &[];
        let signing = ProposalSigningBody {
            kind,
            payload,
            fixtures,
        };
        let signed_bytes = serde_json::to_vec(&signing).expect("signing body");
        let nonce = self.nonce().await;
        let digest = action_digest("proposal", &signed_bytes);
        let challenge = Challenge::new(&nonce, &digest);
        let assertion = self.assertion(&challenge).await;
        let pow = solve(&challenge, 1, &pow_params(), 0).expect("proof-of-work");
        let header = json!({
            "kind": kind,
            "payload": payload,
            "fixtures": serde_json::to_value(fixtures).expect("fixtures json"),
            "pow": pow.as_array().to_vec(),
            "nonce": b64(nonce.as_bytes()),
            "credential_id": b64(&assertion.credential_id),
            "authenticator_data": b64(&assertion.authenticator_data),
            "client_data_json": b64(&assertion.client_data_json),
            "signature": b64(&assertion.signature),
        });
        let header = serde_json::to_vec(&header).expect("header");
        let body = frame_lut_proposal(&header, bytes).expect("frame");
        let request = Request::builder()
            .method("POST")
            .uri("/api/proposals/lut")
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from(body))
            .expect("request");
        let (status, _, body) = send(&self.app, request).await;
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    async fn donate_fixture(&mut self, file: &str, bytes: &[u8]) -> (StatusCode, Value) {
        let sha256 = Sha256Hex::parse(&hex(&Sha256::digest(bytes))).expect("sha256");
        let upload = FixtureUpload {
            file: file.to_owned(),
            sha256,
            size: bytes.len() as u64,
        };
        let signing = serde_json::to_vec(&upload).expect("signing body");
        let nonce = self.nonce().await;
        let digest = action_digest("fixture-upload", &signing);
        let challenge = Challenge::new(&nonce, &digest);
        let assertion = self.assertion(&challenge).await;
        let pow = solve(&challenge, 1, &pow_params(), 0).expect("proof-of-work");
        #[derive(serde::Serialize)]
        struct Header<'a> {
            file: &'a str,
            sha256: Sha256Hex,
            size: u64,
            pow: Vec<u64>,
            nonce: String,
            credential_id: String,
            authenticator_data: String,
            client_data_json: String,
            signature: String,
        }
        let header = serde_json::to_vec(&Header {
            file,
            sha256,
            size: upload.size,
            pow: pow.as_array().to_vec(),
            nonce: b64(nonce.as_bytes()),
            credential_id: b64(&assertion.credential_id),
            authenticator_data: b64(&assertion.authenticator_data),
            client_data_json: b64(&assertion.client_data_json),
            signature: b64(&assertion.signature),
        })
        .expect("header");
        let body = frame_fixture_upload(&header, bytes).expect("frame");
        let request = Request::builder()
            .method("POST")
            .uri("/api/fixtures/donate")
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from(body))
            .expect("request");
        let (status, _, body) = send(&self.app, request).await;
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    async fn moderator_token(&mut self) -> String {
        self.set_credential_tier_roles(&self.credential_id, "new", "[\"moderator\"]")
            .await;
        let signed = self.signed_fields("mod-session", &[]).await;
        let (status, session) = post_json(&self.app, "/api/mod/session", &signed).await;
        assert_eq!(status, StatusCode::OK, "session: {session}");
        session["session_token"]
            .as_str()
            .expect("session token")
            .to_owned()
    }

    async fn decide(&self, token: &str, proposal: &str, reason: &str) -> (StatusCode, Value) {
        self.decide_with(token, proposal, "approve", reason).await
    }

    async fn decide_with(
        &self,
        token: &str,
        proposal: &str,
        decision: &str,
        reason: &str,
    ) -> (StatusCode, Value) {
        let decision = json!({
            "proposal_id": proposal,
            "decision": decision,
            "reason": reason,
        });
        let request = Request::builder()
            .method("POST")
            .uri("/api/mod/decision")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(
                serde_json::to_vec(&decision).expect("decision json"),
            ))
            .expect("request");
        let (status, _, body) = send(&self.app, request).await;
        let value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        (status, value)
    }

    async fn revert_look(&self, token: &str, payload: Value) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("POST")
            .uri("/api/mod/look/revert")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(
                serde_json::to_vec(&payload).expect("revert json"),
            ))
            .expect("request");
        let (status, _, body) = send(&self.app, request).await;
        let value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        (status, value)
    }

    async fn confirm_proposal(
        &mut self,
        proposal: &str,
        tags: Vec<LabeledTag>,
    ) -> (StatusCode, Value) {
        let signing = ConfirmationSigningBody {
            proposal_id: proposal,
            tags: &tags,
        };
        let bytes = serde_json::to_vec(&signing).expect("signing body");
        let nonce = self.nonce().await;
        let digest = action_digest("proposal-confirmation", &bytes);
        let challenge = Challenge::new(&nonce, &digest);
        let assertion = self.assertion(&challenge).await;
        let body = json!({
            "proposal_id": proposal,
            "tags": serde_json::to_value(&tags).expect("tags json"),
            "nonce": b64(nonce.as_bytes()),
            "credential_id": b64(&assertion.credential_id),
            "authenticator_data": b64(&assertion.authenticator_data),
            "client_data_json": b64(&assertion.client_data_json),
            "signature": b64(&assertion.signature),
        });
        post_json(&self.app, "/api/proposals/confirm", &body).await
    }

    async fn read_thread(&mut self, proposal: &str) -> (StatusCode, Value) {
        let signing = ThreadSigningBody {
            proposal_id: proposal,
        };
        let bytes = serde_json::to_vec(&signing).expect("signing body");
        let nonce = self.nonce().await;
        let digest = action_digest("proposal-thread", &bytes);
        let challenge = Challenge::new(&nonce, &digest);
        let assertion = self.assertion(&challenge).await;
        let body = json!({
            "proposal_id": proposal,
            "nonce": b64(nonce.as_bytes()),
            "credential_id": b64(&assertion.credential_id),
            "authenticator_data": b64(&assertion.authenticator_data),
            "client_data_json": b64(&assertion.client_data_json),
            "signature": b64(&assertion.signature),
        });
        post_json(&self.app, "/api/proposals/thread", &body).await
    }

    async fn post_proposer_message(
        &mut self,
        proposal: &str,
        message: &str,
    ) -> (StatusCode, Value) {
        let signing = MessageSigningBody {
            proposal_id: proposal,
            body: message,
        };
        let bytes = serde_json::to_vec(&signing).expect("signing body");
        let nonce = self.nonce().await;
        let digest = action_digest("proposal-thread", &bytes);
        let challenge = Challenge::new(&nonce, &digest);
        let assertion = self.assertion(&challenge).await;
        let body = json!({
            "proposal_id": proposal,
            "body": message,
            "nonce": b64(nonce.as_bytes()),
            "credential_id": b64(&assertion.credential_id),
            "authenticator_data": b64(&assertion.authenticator_data),
            "client_data_json": b64(&assertion.client_data_json),
            "signature": b64(&assertion.signature),
        });
        post_json(&self.app, "/api/proposals/message", &body).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn proposal_decision_writes_a_validated_snapshot() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");

    let body_yaml = include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml");
    let (status, body) = server.submit_proposal("body", body_yaml, &[], false).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "bad pow: {body}");

    let (status, body) = server.submit_proposal("body", body_yaml, &[], true).await;
    assert_eq!(status, StatusCode::CREATED, "submit: {body}");
    let proposal = body["view"]["id"].as_str().expect("proposal id").to_owned();
    assert_eq!(
        body["view"]["summary"]["headline"],
        "adds Fujifilm X-T5 as a camera body"
    );
    assert_eq!(body["view"]["status"], "pending");
    assert!(body["round_trip"].is_null());

    let tags = vec![LabeledTag {
        tag: "FilmMode".try_into().expect("tag"),
        value: MakernoteValue(0x0700),
        label: "classic_chrome".try_into().expect("label"),
    }];
    let (status, body) = server.confirm_proposal(&proposal, tags).await;
    assert_eq!(status, StatusCode::OK, "confirm: {body}");
    assert_eq!(body["evidence"].as_array().expect("evidence").len(), 1);

    let (status, body) = server
        .post_proposer_message(&proposal, "firmware 2.00")
        .await;
    assert_eq!(status, StatusCode::OK, "message: {body}");

    server
        .set_credential_tier_roles(&server.credential_id, "new", "[\"moderator\"]")
        .await;
    let signed = server.signed_fields("mod-session", &[]).await;
    let (status, session) = post_json(&server.app, "/api/mod/session", &signed).await;
    assert_eq!(status, StatusCode::OK, "session: {session}");
    let token = session["session_token"].as_str().expect("token").to_owned();

    let request = Request::builder()
        .method("GET")
        .uri(format!("/api/mod/proposal/{proposal}"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request");
    let (status, _, body) = send(&server.app, request).await;
    assert_eq!(status, StatusCode::OK, "mod view: {body:?}");
    let record: Value = serde_json::from_slice(&body).expect("mod view json");
    assert_eq!(record["thread"].as_array().expect("thread").len(), 1);
    assert_eq!(record["thread"][0]["author"], "proposer");

    let message = json!({ "proposal_id": proposal, "body": "please add the fixture" });
    let request = Request::builder()
        .method("POST")
        .uri("/api/mod/proposal-message")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&message).expect("message")))
        .expect("request");
    let (status, _, body) = send(&server.app, request).await;
    assert_eq!(status, StatusCode::OK, "mod message: {body:?}");

    let (status, thread) = server.read_thread(&proposal).await;
    assert_eq!(status, StatusCode::OK, "thread: {thread}");
    let messages = thread["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1]["author"], "moderator");

    let decision = json!({
        "proposal_id": proposal,
        "decision": "approve",
        "reason": "validated locally",
    });
    let request = Request::builder()
        .method("POST")
        .uri("/api/mod/decision")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&decision).expect("decision")))
        .expect("request");
    let (status, _, body) = send(&server.app, request).await;
    assert_eq!(status, StatusCode::OK, "decision: {body:?}");
    let decided: Value = serde_json::from_slice(&body).expect("decision json");
    assert_eq!(decided["snapshot"]["version"], 1);
    let hash = decided["snapshot"]["content_hash"]
        .as_str()
        .expect("content hash");
    assert_eq!(hash.len(), 64);

    let snapshot = server.dir.path().join("community/v1");
    assert!(
        looklab_schema::validate_snapshot_dir(&snapshot)
            .expect("snapshot walk")
            .is_empty()
    );
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(snapshot.join("snapshot.json")).expect("manifest"),
    )
    .expect("manifest json");
    assert_eq!(manifest["content_hash"], hash);
    let body = looklab_schema::validate_body(body_yaml).expect("body");
    assert!(
        snapshot
            .join(looklab_schema::body_export_path(&body.id))
            .is_file()
    );
    assert!(
        snapshot
            .join(looklab_schema::body_export_path(
                &looklab_schema::BodyId::new("fujifilm/x-t4").expect("body id")
            ))
            .is_file()
    );
    let profile = looklab_schema::validate_interface_profile(include_str!(
        "../../../data/interfaces/example-app-lut3d-v1.yaml"
    ))
    .expect("interface");
    assert!(
        snapshot
            .join(looklab_schema::interface_export_path(&profile.id))
            .is_file()
    );

    let (published, blobs) = manifest_at(CommunitySnapshotVersion::new(1), valid_settings());
    let (status, body) = server.upload_manifest(&published, &blobs, None).await;
    assert_eq!(status, StatusCode::CREATED, "v1 upload: {body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_requests_a_catalog_publication_without_a_fit() {
    let mut server = TestServer::start().await;
    let before: i64 = sqlx::query_scalar("SELECT generation FROM publication WHERE id = 1")
        .fetch_one(&server.db)
        .await
        .expect("publication generation");
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");

    let (status, body) = server
        .submit_proposal(
            "body",
            include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml"),
            &[],
            true,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "submit: {body}");
    let proposal = body["view"]["id"].as_str().expect("proposal id").to_owned();

    let token = server.moderator_token().await;
    let (status, body) = server.decide(&token, &proposal, "validated locally").await;
    assert_eq!(status, StatusCode::OK, "decision: {body}");

    let generation: i64 = sqlx::query_scalar("SELECT generation FROM publication WHERE id = 1")
        .fetch_one(&server.db)
        .await
        .expect("publication generation");
    assert_eq!(
        generation,
        before + 1,
        "approval records a publication request"
    );
    let queued: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE kind = 'publish_site' AND status = 'queued'",
    )
    .fetch_one(&server.db)
    .await
    .expect("publish job");
    assert_eq!(queued, 1, "approval queues a catalog publisher");
}

#[tokio::test(flavor = "multi_thread")]
async fn fixture_donations_export_and_interface_profiles_carry_a_report() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");

    let mapping = "\
body: fujifilm/x-t5
epoch: 1
sim_key: reala_ace
look: reala-ace
firmware_from: \"1.00\"
makernote: { tag: FilmMode, value: 1792 }
";
    let fixture = FixtureDonation {
        file: "fujifilm-x-t5-1.00.raf".to_owned(),
        description: "X-T5 firmware 1.00, Reala Ace".to_owned(),
        make: "Fujifilm".to_owned(),
        model: "X-T5".to_owned(),
        sha256: Sha256Hex::parse(&"ab".repeat(32)).expect("sha256"),
        size: 24_000_000,
        source_url: Some("https://example.com/fujifilm-x-t5-1.00.raf".to_owned()),
        source: None,
        donator: Some("tester".to_owned()),
        donated_on: Date::parse("2026-11-04").expect("date"),
        license: FixtureLicense::Cc0_1_0,
        placement: Placement::Fetch,
    };
    let (status, body) = server
        .submit_proposal("mapping", mapping, std::slice::from_ref(&fixture), true)
        .await;
    assert_eq!(status, StatusCode::CREATED, "mapping: {body}");
    assert_eq!(
        body["view"]["summary"]["headline"],
        "adds Reala Ace to X-T5 firmware 1.00+"
    );
    let proposal = body["view"]["id"].as_str().expect("proposal id").to_owned();

    let profile = include_str!("../../../data/interfaces/example-app-lut3d-v1.yaml");
    let (status, body) = server
        .submit_proposal("interface_profile", profile, &[], true)
        .await;
    assert_eq!(status, StatusCode::CREATED, "interface: {body}");
    let report = &body["round_trip"];
    assert!(report["probes"].as_u64().is_some(), "{report}");
    assert!(!report["ccts"].as_array().expect("ccts").is_empty());

    server
        .set_credential_tier_roles(&server.credential_id, "new", "[\"moderator\"]")
        .await;
    let signed = server.signed_fields("mod-session", &[]).await;
    let (status, session) = post_json(&server.app, "/api/mod/session", &signed).await;
    assert_eq!(status, StatusCode::OK, "session: {session}");
    let token = session["session_token"].as_str().expect("token").to_owned();
    let decision = json!({
        "proposal_id": proposal,
        "decision": "approve",
        "reason": "fixture matches the label",
    });
    let request = Request::builder()
        .method("POST")
        .uri("/api/mod/decision")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&decision).expect("decision")))
        .expect("request");
    let (status, _, body) = send(&server.app, request).await;
    assert_eq!(status, StatusCode::OK, "decision: {body:?}");
    let decided: Value = serde_json::from_slice(&body).expect("decision json");
    assert_eq!(decided["snapshot"]["version"], 1);

    let snapshot = server.dir.path().join("community/v1");
    let failures = looklab_schema::validate_snapshot_dir(&snapshot).expect("snapshot walk");
    assert!(failures.is_empty(), "{failures:?}");
    let schema = looklab_schema::validate_control_schema(
        &std::fs::read_to_string(snapshot.join(looklab_schema::schema_export_path(
            &"fujifilm/x-t5".try_into().expect("body id"),
        )))
        .expect("updated schema"),
    )
    .expect("valid schema");
    let reala = schema.epochs[0]
        .native_sims
        .iter()
        .find(|sim| sim.key.as_str() == "reala_ace")
        .expect("the mapping was applied");
    assert_eq!(reala.look.as_str(), "reala-ace");
    assert_eq!(reala.makernote.tag.as_str(), "FilmMode");
    assert_eq!(reala.makernote.value, looklab_schema::MakernoteValue(1792));

    let (status, _, body) = server.get("/api/fixtures/manifest.toml").await;
    assert_eq!(status, StatusCode::OK);
    let manifest = String::from_utf8(body.to_vec()).expect("toml");
    assert!(manifest.contains("[[fixture]]"), "{manifest}");
    assert!(manifest.contains("file = \"fujifilm-x-t5-1.00.raf\""));
    assert!(manifest.contains("license = \"CC0-1.0\""));
    assert!(manifest.contains("Donated by tester on 2026-11-04"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_proposal_body_cannot_be_replayed() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
    let profile = include_str!("../../../data/interfaces/example-app-lut3d-v1.yaml");
    let body = server
        .signed_proposal_body(ProposalKind::InterfaceProfile, profile, &[], true)
        .await;
    let (status, response) = post_json(&server.app, "/api/proposals", &body).await;
    assert_eq!(status, StatusCode::CREATED, "first: {response}");
    let (status, response) = post_json(&server.app, "/api/proposals", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "replay: {response}");
    assert_eq!(response["error"], "NONCE_REPLAYED");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_fixture_donation_keeps_its_file_for_the_worker_sweep() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
    let bytes = b"looklab fixture bytes";
    sqlx::query(
        "CREATE TRIGGER reject_fixture BEFORE INSERT ON fixture_uploads
         BEGIN SELECT RAISE(ABORT, 'reject'); END",
    )
    .execute(&server.db)
    .await
    .expect("trigger");

    let (status, body) = server.donate_fixture("looklab-test.raf", bytes).await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "donation: {body}"
    );
    let hash = hex(&Sha256::digest(bytes));
    assert!(server.blob_dir.join(&hash).exists());
    let staged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blob_staging")
        .fetch_one(&server.db)
        .await
        .expect("reservation");
    assert_eq!(staged, 1);
    let fixtures: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fixture_uploads")
        .fetch_one(&server.db)
        .await
        .expect("fixture rows");
    assert_eq!(fixtures, 0);

    sqlx::query("DROP TRIGGER reject_fixture")
        .execute(&server.db)
        .await
        .expect("drop trigger");
    let (status, body) = server.donate_fixture("looklab-test.raf", bytes).await;
    assert_eq!(status, StatusCode::CREATED, "retry: {body}");
    assert!(server.blob_dir.join(&hash).exists());
    let fixtures: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fixture_uploads")
        .fetch_one(&server.db)
        .await
        .expect("fixture rows");
    assert_eq!(fixtures, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_uploaded_fixture_is_exported_on_approval() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");

    let bytes = b"looklab fixture bytes";
    let (status, body) = server.donate_fixture("looklab-test.raf", bytes).await;
    assert_eq!(status, StatusCode::CREATED, "donation: {body}");
    let source = body["source"].as_str().expect("source").to_owned();
    let fixture = FixtureDonation {
        file: "looklab-test.raf".to_owned(),
        description: "A donated test fixture".to_owned(),
        make: "Fujifilm".to_owned(),
        model: "X-T5".to_owned(),
        sha256: Sha256Hex::parse(&hex(&Sha256::digest(bytes))).expect("sha256"),
        size: bytes.len() as u64,
        source_url: None,
        source: Some(source),
        donator: Some("tester".to_owned()),
        donated_on: Date::parse("2026-11-04").expect("date"),
        license: FixtureLicense::Cc0_1_0,
        placement: Placement::Fetch,
    };
    let body_yaml = include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml");
    let (status, body) = server
        .submit_proposal("body", body_yaml, std::slice::from_ref(&fixture), true)
        .await;
    assert_eq!(status, StatusCode::CREATED, "submit: {body}");
    let proposal = body["view"]["id"].as_str().expect("proposal id").to_owned();

    let token = server.moderator_token().await;
    let (status, body) = server
        .decide(&token, &proposal, "the fixture is donated")
        .await;
    assert_eq!(status, StatusCode::OK, "decision: {body}");
    assert_eq!(body["snapshot"]["version"], 1);

    let snapshot = server.dir.path().join("community/v1");
    let exported = snapshot.join("fixtures/donated/looklab-test.raf");
    assert_eq!(
        std::fs::read(&exported).expect("exported fixture"),
        bytes,
        "a published donation is exported under its LFS path"
    );
    assert!(
        !snapshot.join("fixtures/looklab-test.raf").exists(),
        "a donated RAW must not be exported into plain git"
    );
    let failures = looklab_schema::validate_snapshot_dir(&snapshot).expect("snapshot walk");
    assert!(failures.is_empty(), "{failures:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fixture_source_needs_an_upload_by_the_proposer() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");

    let bytes = b"never uploaded";
    let sha256 = Sha256Hex::parse(&hex(&Sha256::digest(bytes))).expect("sha256");
    let fixture = FixtureDonation {
        file: "looklab-missing.raf".to_owned(),
        description: "A fixture nobody uploaded".to_owned(),
        make: "Fujifilm".to_owned(),
        model: "X-T5".to_owned(),
        sha256,
        size: bytes.len() as u64,
        source_url: None,
        source: Some(looklab_schema::fixture_source(sha256)),
        donator: None,
        donated_on: Date::parse("2026-11-04").expect("date"),
        license: FixtureLicense::Cc0_1_0,
        placement: Placement::Fetch,
    };
    let body_yaml = include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml");
    let (status, body) = server
        .submit_proposal("body", body_yaml, std::slice::from_ref(&fixture), true)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "submit: {body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mapping_approval_without_a_body_schema_is_not_committed() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");

    let mapping = "\
body: fujifilm/x-unknown
epoch: 1
sim_key: reala_ace
look: reala-ace
firmware_from: \"1.00\"
makernote: { tag: FilmMode, value: 1792 }
";
    let (status, body) = server.submit_proposal("mapping", mapping, &[], true).await;
    assert_eq!(status, StatusCode::CREATED, "mapping: {body}");
    let proposal = body["view"]["id"].as_str().expect("proposal id").to_owned();

    let token = server.moderator_token().await;
    let (status, body) = server.decide(&token, &proposal, "the body is known").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "decision: {body}");
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM proposals WHERE id = ?")
            .bind(&proposal)
            .fetch_one(&server.db)
            .await
            .expect("proposal status"),
        "pending"
    );
    assert!(!server.dir.path().join("community/v1").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_first_schema_needs_a_registered_body_and_then_publishes() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");

    let (status, _, body) = server.get("/api/body-context").await;
    assert_eq!(status, StatusCode::OK, "body context: {body:?}");
    let context: Value = serde_json::from_slice(&body).expect("body context json");
    assert!(
        !context["bodies"]
            .as_array()
            .expect("bodies")
            .iter()
            .any(|body| body["id"] == "fujifilm/x-m1")
    );

    let schema = "\
body: fujifilm/x-m1
epochs:
- id: 1
  firmware: { from: \"1.00\", to: \"1.99\" }
  native_sims: []
  exposure_modes: []
  controls: []
  solve_plan: []
";
    let (status, body) = server
        .submit_proposal("schema_change", schema, &[], true)
        .await;
    assert_eq!(status, StatusCode::CREATED, "schema submit: {body}");
    let schema_proposal = body["view"]["id"].as_str().expect("proposal id").to_owned();
    let token = server.moderator_token().await;
    let (status, body) = server
        .decide(&token, &schema_proposal, "the body is not registered")
        .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "schema before body: {body}"
    );
    assert!(!server.dir.path().join("community/v1").exists());

    let body_yaml = "\
id: fujifilm/x-m1
make: Fujifilm
model: X-M1
exif_aliases:
- X-M1
kind: camera
capabilities:
  in_camera_raw_conversion: true
  jpeg_color_spaces: []
  heif: false
status: proposed
mapping_status: provisional
";
    let (status, body) = server.submit_proposal("body", body_yaml, &[], true).await;
    assert_eq!(status, StatusCode::CREATED, "body submit: {body}");
    let body_proposal = body["view"]["id"].as_str().expect("proposal id").to_owned();
    let (status, body) = server
        .decide(&token, &body_proposal, "register the body")
        .await;
    assert_eq!(status, StatusCode::OK, "body decision: {body}");
    assert_eq!(body["snapshot"]["version"], 1);

    let (status, _, body) = server.get("/api/body-context").await;
    assert_eq!(status, StatusCode::OK, "body context: {body:?}");
    let context: Value = serde_json::from_slice(&body).expect("body context json");
    assert!(
        context["bodies"]
            .as_array()
            .expect("bodies")
            .iter()
            .any(|body| body["id"] == "fujifilm/x-m1")
    );

    let (status, body) = server
        .submit_proposal("schema_change", schema, &[], true)
        .await;
    assert_eq!(status, StatusCode::CREATED, "schema submit: {body}");
    let schema_proposal = body["view"]["id"].as_str().expect("proposal id").to_owned();
    let (status, body) = server
        .decide(
            &token,
            &schema_proposal,
            "first schema for the registered body",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "schema decision: {body}");
    assert_eq!(body["snapshot"]["version"], 2);
    let body_id = BodyId::new("fujifilm/x-m1").expect("body id");
    assert!(
        server
            .dir
            .path()
            .join("community/v2")
            .join(looklab_schema::schema_export_path(&body_id))
            .is_file()
    );

    let (status, _, body) = server.get("/api/body-context").await;
    assert_eq!(status, StatusCode::OK, "body context: {body:?}");
    let context: Value = serde_json::from_slice(&body).expect("body context json");
    assert!(
        !context["bodies"]
            .as_array()
            .expect("bodies")
            .iter()
            .any(|body| body["id"] == "fujifilm/x-m1")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_first_schema_reaches_a_body_shipped_after_the_active_snapshot() {
    let mut server = TestServer::start_with_shipped_copy().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");

    let (status, body) = server
        .submit_proposal(
            "body",
            include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml"),
            &[],
            true,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "body submit: {body}");
    let body_proposal = body["view"]["id"].as_str().expect("proposal id").to_owned();
    let token = server.moderator_token().await;
    let (status, body) = server
        .decide(&token, &body_proposal, "seed the first snapshot")
        .await;
    assert_eq!(status, StatusCode::OK, "body decision: {body}");
    assert_eq!(body["snapshot"]["version"], 1);

    std::fs::write(
        server
            .state
            .config
            .bodies_dir
            .join("fujifilm-x-m2.body.yaml"),
        "\
id: fujifilm/x-m2
make: Fujifilm
model: X-M2
exif_aliases:
- X-M2
kind: camera
capabilities:
  in_camera_raw_conversion: true
  jpeg_color_spaces: []
  heif: false
status: proposed
mapping_status: provisional
",
    )
    .expect("shipped body entity");

    let (status, _, body) = server.get("/api/body-context").await;
    assert_eq!(status, StatusCode::OK, "body context: {body:?}");
    let context: Value = serde_json::from_slice(&body).expect("body context json");
    assert!(
        context["bodies"]
            .as_array()
            .expect("bodies")
            .iter()
            .any(|body| body["id"] == "fujifilm/x-m2"),
        "the shipped registration is offered for a first schema"
    );

    let schema = "\
body: fujifilm/x-m2
epochs:
- id: 1
  firmware: { from: \"1.00\", to: \"1.99\" }
  native_sims: []
  exposure_modes: []
  controls: []
  solve_plan: []
";
    let (status, body) = server
        .submit_proposal("schema_change", schema, &[], true)
        .await;
    assert_eq!(status, StatusCode::CREATED, "schema submit: {body}");
    let schema_proposal = body["view"]["id"].as_str().expect("proposal id").to_owned();
    let (status, body) = server
        .decide(
            &token,
            &schema_proposal,
            "first schema for the shipped body",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "schema decision: {body}");
    assert_eq!(body["snapshot"]["version"], 2);

    let body_id = BodyId::new("fujifilm/x-m2").expect("body id");
    let v2 = server.dir.path().join("community/v2");
    assert!(
        v2.join(looklab_schema::body_export_path(&body_id))
            .is_file(),
        "the new snapshot carries the shipped registration"
    );
    assert!(
        v2.join(looklab_schema::schema_export_path(&body_id))
            .is_file()
    );
    let failures = looklab_schema::validate_snapshot_dir(&v2).expect("snapshot walk");
    assert!(failures.is_empty(), "{failures:?}");
    assert!(
        !server
            .dir
            .path()
            .join("community/v1")
            .join(looklab_schema::body_export_path(&body_id))
            .exists(),
        "the older snapshot keeps its documents"
    );

    let (status, _, body) = server.get("/api/body-context").await;
    assert_eq!(status, StatusCode::OK, "body context: {body:?}");
    let context: Value = serde_json::from_slice(&body).expect("body context json");
    assert!(
        !context["bodies"]
            .as_array()
            .expect("bodies")
            .iter()
            .any(|body| body["id"] == "fujifilm/x-m2")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approved_snapshot_survives_a_failed_publication_transition() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
    let payload = include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml");
    let (status, body) = server.submit_proposal("body", payload, &[], true).await;
    assert_eq!(status, StatusCode::CREATED, "submit: {body}");
    let proposal = body["view"]["id"].as_str().expect("proposal id").to_owned();
    let token = server.moderator_token().await;
    sqlx::query(
        "CREATE TRIGGER fail_snapshot_transition
         BEFORE UPDATE OF publication ON proposals
         WHEN NEW.publication = 'published'
         BEGIN SELECT RAISE(ABORT, 'publication fault'); END",
    )
    .execute(&server.db)
    .await
    .expect("publication fault");
    let (status, body) = server.decide(&token, &proposal, "validated locally").await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "decision: {body}"
    );
    let activation: (i64, String) = sqlx::query_as(
        "SELECT version, content_hash FROM community_snapshot_activation WHERE id = 1",
    )
    .fetch_one(&server.db)
    .await
    .expect("active snapshot");
    assert_eq!(activation.0, 1);
    let recorded: (i64, String) =
        sqlx::query_as("SELECT published_version, published_hash FROM proposals WHERE id = ?")
            .bind(&proposal)
            .fetch_one(&server.db)
            .await
            .expect("recorded snapshot identity");
    assert_eq!(recorded.0, activation.0);
    assert_eq!(recorded.1, activation.1);

    let (approved, blobs) = manifest_at(CommunitySnapshotVersion::new(1), valid_settings());
    let (status, body) = server.upload_manifest(&approved, &blobs, None).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "approved snapshot upload: {body}"
    );

    sqlx::query("DROP TRIGGER fail_snapshot_transition")
        .execute(&server.db)
        .await
        .expect("remove publication fault");
    let (status, body) = server.submit_proposal("body", payload, &[], true).await;
    assert_eq!(status, StatusCode::CREATED, "next submit: {body}");
    let proposal = body["view"]["id"].as_str().expect("proposal id").to_owned();
    let (status, body) = server.decide(&token, &proposal, "validated locally").await;
    assert_eq!(status, StatusCode::OK, "next decision: {body}");
    assert_eq!(body["snapshot"]["version"], 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_activation_keeps_the_target_unexposed() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
    let payload = include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml");
    let (status, body) = server.submit_proposal("body", payload, &[], true).await;
    assert_eq!(status, StatusCode::CREATED, "submit: {body}");
    let proposal = body["view"]["id"].as_str().expect("proposal id").to_owned();
    let token = server.moderator_token().await;
    sqlx::query(
        "CREATE TRIGGER fail_snapshot_activation
         BEFORE INSERT ON community_snapshot_activation
         BEGIN SELECT RAISE(ABORT, 'activation fault'); END",
    )
    .execute(&server.db)
    .await
    .expect("activation fault");
    let (status, body) = server.decide(&token, &proposal, "validated locally").await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "decision: {body}"
    );
    let active: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM community_snapshot_activation")
        .fetch_one(&server.db)
        .await
        .expect("activation rows");
    assert_eq!(active, 0);
    assert!(server.dir.path().join("community/v1").is_dir());
    let (approved, blobs) = manifest_at(CommunitySnapshotVersion::new(1), valid_settings());
    let (status, body) = server.upload_manifest(&approved, &blobs, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "unexposed: {body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_older_snapshot_does_not_replace_a_newer_activation() {
    let mut server = TestServer::start().await;
    sqlx::query(
        "INSERT INTO community_snapshot_activation
         (id, version, content_hash, activated_at)
         VALUES (1, 2, ?, 0)",
    )
    .bind("0".repeat(64))
    .execute(&server.db)
    .await
    .expect("newer activation");
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
    let payload = include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml");
    let (status, body) = server.submit_proposal("body", payload, &[], true).await;
    assert_eq!(status, StatusCode::CREATED, "submit: {body}");
    let proposal = body["view"]["id"].as_str().expect("proposal id").to_owned();
    let token = server.moderator_token().await;
    let (status, body) = server.decide(&token, &proposal, "validated locally").await;
    assert_eq!(status, StatusCode::OK, "decision: {body}");
    let active: i64 =
        sqlx::query_scalar("SELECT version FROM community_snapshot_activation WHERE id = 1")
            .fetch_one(&server.db)
            .await
            .expect("active snapshot");
    assert_eq!(active, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unrelated_reload_fault_preserves_the_approved_target() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
    let payload = include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml");
    let token = server.moderator_token().await;
    for (index, expected) in [("first", 1), ("second", 2)] {
        let (status, body) = server.submit_proposal("body", payload, &[], true).await;
        assert_eq!(status, StatusCode::CREATED, "{index} submit: {body}");
        let proposal = body["view"]["id"].as_str().expect("proposal id").to_owned();
        let (status, body) = server.decide(&token, &proposal, "validated locally").await;
        assert_eq!(status, StatusCode::OK, "{index} decision: {body}");
        assert_eq!(body["snapshot"]["version"], expected);
    }
    let v1 = server.dir.path().join("community/v1");
    assert!(v1.is_dir());
    std::fs::write(v1.join("bodies/fujifilm-x-t5.yaml"), b"not: a schema\n")
        .expect("corrupt retained snapshot");

    let (status, body) = server.submit_proposal("body", payload, &[], true).await;
    assert_eq!(status, StatusCode::CREATED, "third submit: {body}");
    let proposal = body["view"]["id"].as_str().expect("proposal id").to_owned();
    let (status, body) = server.decide(&token, &proposal, "validated locally").await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "third decision: {body}"
    );
    assert!(server.dir.path().join("community/v3").is_dir());
    let active: i64 =
        sqlx::query_scalar("SELECT version FROM community_snapshot_activation WHERE id = 1")
            .fetch_one(&server.db)
            .await
            .expect("active snapshot");
    assert_eq!(active, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pending_publication_is_retried_before_the_next_decision() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
    let actor = server.add_credential().await;

    let body_yaml = include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml");
    let body = looklab_schema::validate_body(body_yaml).expect("body");
    let mut files = shipped_baseline_files();
    files.retain(|(path, _)| path != &looklab_schema::body_export_path(&body.id));
    files.push((
        looklab_schema::body_export_path(&body.id),
        body_yaml.as_bytes().to_vec(),
    ));
    files.push((
        looklab_schema::FIXTURE_MANIFEST.to_owned(),
        looklab_schema::fixture_manifest_toml(&[]).into_bytes(),
    ));
    let hash = looklab_schema::content_hash(&files);
    let unpublished = looklab_server::ProposalId::new();
    sqlx::query(
        "INSERT INTO proposals
         (id, credential_id, type, payload, human_summary, evidence, status, created_at,
          publication, published_version, published_hash)
         VALUES (?, ?, 'body', ?, 'adds a body', '[]', 'approved', 0, 'pending', 1, ?)",
    )
    .bind(unpublished.as_str())
    .bind(&actor)
    .bind(body_yaml)
    .bind(&hash)
    .execute(&server.db)
    .await
    .expect("pending publication");

    let (status, body) = server.submit_proposal("body", body_yaml, &[], true).await;
    assert_eq!(status, StatusCode::CREATED, "submit: {body}");
    let proposal = body["view"]["id"].as_str().expect("proposal id").to_owned();
    let token = server.moderator_token().await;
    let (status, body) = server.decide(&token, &proposal, "validated locally").await;
    assert_eq!(status, StatusCode::OK, "decision: {body}");
    assert_eq!(body["snapshot"]["version"], 2);

    for version in [1, 2] {
        let dir = server.dir.path().join(format!("community/v{version}"));
        let failures = looklab_schema::validate_snapshot_dir(&dir).expect("snapshot walk");
        assert!(failures.is_empty(), "v{version}: {failures:?}");
    }
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT publication FROM proposals WHERE id = ?")
            .bind(unpublished.as_str())
            .fetch_one(&server.db)
            .await
            .expect("publication state"),
        "published"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fixture_backed_body_needs_a_fixture_donation() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");

    let body_yaml = include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml").replace(
        "mapping_status: provisional",
        "mapping_status: fixture_backed",
    );
    let (status, body) = server.submit_proposal("body", &body_yaml, &[], true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "claimed status: {body}");

    let donation = FixtureDonation {
        file: "fujifilm-x-s10-1.00.raf".to_owned(),
        description: "X-S10 firmware 1.00".to_owned(),
        make: "Fujifilm".to_owned(),
        model: "X-S10".to_owned(),
        sha256: Sha256Hex::parse(&"cd".repeat(32)).expect("sha256"),
        size: 24_000_000,
        source_url: Some("https://example.com/fujifilm-x-s10-1.00.raf".to_owned()),
        source: None,
        donator: None,
        donated_on: Date::parse("2026-11-04").expect("date"),
        license: FixtureLicense::Cc0_1_0,
        placement: Placement::Fetch,
    };
    let (status, body) = server
        .submit_proposal("body", &body_yaml, std::slice::from_ref(&donation), true)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "another body: {body}");

    let donation = FixtureDonation {
        file: "fujifilm-x-t5-1.00.raf".to_owned(),
        model: "X-T5".to_owned(),
        ..donation
    };
    let (status, body) = server
        .submit_proposal("body", &body_yaml, std::slice::from_ref(&donation), true)
        .await;
    assert_eq!(status, StatusCode::CREATED, "matching donation: {body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interface_profile_with_unbounded_lut_grids_is_rejected() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
    let profile = include_str!("../../../data/interfaces/example-app-lut3d-v1.yaml");

    let oversized = profile.replace("sizes: [17, 33]", "sizes: [70000]");
    let (status, body) = server
        .submit_proposal("interface_profile", &oversized, &[], true)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "oversized side: {body}");

    let over_budget = profile.replace("sizes: [17, 33]", "sizes: [17, 33, 65, 129, 256]");
    let (status, body) = server
        .submit_proposal("interface_profile", &over_budget, &[], true)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "node budget: {body}");

    let (status, body) = server
        .submit_proposal("interface_profile", profile, &[], true)
        .await;
    assert_eq!(status, StatusCode::CREATED, "accepted profile: {body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn yaml_anchors_and_aliases_are_rejected_at_intake() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");

    let anchored = "\
id: fujifilm/x-t9
make: Fujifilm
model: X-T9
exif_aliases: *names
kind: camera
capabilities:
  in_camera_raw_conversion: true
  jpeg_color_spaces: &names [srgb]
  heif: false
status: proposed
mapping_status: provisional
";
    let (status, body) = server.submit_proposal("body", anchored, &[], true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "aliased body: {body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_continuation_keeps_the_newer_assertion_state() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;

    let (first, first_blobs) = manifest(valid_settings());
    let (mut second, second_blobs) = manifest(valid_settings());
    second.sim_key = SimKey::new("provia").expect("sim key");
    let nonce = server.nonce().await;
    let send = SendManifest::canonical(vec![manifest_hash(&first), manifest_hash(&second)])
        .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = server.assertion(&challenge).await;
    assert_eq!(
        server
            .upload_bytes(send_envelope(
                nonce,
                &send,
                &first,
                &first_blobs,
                &assertion
            ))
            .await
            .0,
        StatusCode::CREATED
    );

    let newer = signed_session_request(&mut server, RequestDomain::Session).await;
    let (status, body) = post_json(&server.app, "/api/session", &newer).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(
        server
            .upload_bytes(send_envelope(
                nonce,
                &send,
                &second,
                &second_blobs,
                &assertion
            ))
            .await
            .0,
        StatusCode::CREATED
    );

    let credential_json: String = sqlx::query_scalar("SELECT webauthn_credential FROM credentials")
        .fetch_one(&server.db)
        .await
        .expect("credential");
    let passkey: webauthn_rs::prelude::Passkey =
        serde_json::from_str(&credential_json).expect("stored passkey");
    let credential: webauthn_rs::prelude::Credential = passkey.into();
    assert_eq!(credential.counter, 2);

    // SoftPasskey cannot emit two assertions with the same counter, so the
    // equal-counter transition is covered by the store CAS tests.
    let follow_on = signed_session_request(&mut server, RequestDomain::Session).await;
    let (status, body) = post_json(&server.app, "/api/session", &follow_on).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

struct SyncedPasskey {
    key: SigningKey,
    credential_id: Vec<u8>,
}

impl SyncedPasskey {
    fn new(credential_id: &[u8]) -> Self {
        Self {
            key: SigningKey::from_slice(&[9u8; 32]).expect("valid P-256 scalar"),
            credential_id: credential_id.to_vec(),
        }
    }

    async fn register(&self, app: &Router, display_handle: &str, flags: u8) {
        let (status, body) = self.register_response(app, display_handle, flags).await;
        assert_eq!(status, StatusCode::OK, "register finish: {body}");
    }

    async fn register_response(
        &self,
        app: &Router,
        display_handle: &str,
        flags: u8,
    ) -> (StatusCode, Value) {
        let (status, body) = post_json(
            app,
            "/api/passkey/register/start",
            &json!({ "display_handle": display_handle }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "register start: {body}");
        let handle = body["handle"].as_str().expect("handle").to_owned();
        let challenge = body["options"]["publicKey"]["challenge"]
            .as_str()
            .expect("challenge");
        let client_data_json = format!(
            "{{\"type\":\"webauthn.create\",\"challenge\":\"{challenge}\",\"origin\":\"{ORIGIN}\",\"crossOrigin\":false}}"
        );
        let mut auth_data = Vec::new();
        auth_data.extend_from_slice(&Sha256::digest(RP_ID.as_bytes()));
        auth_data.push(flags);
        auth_data.extend_from_slice(&0u32.to_be_bytes());
        auth_data.extend_from_slice(&[0u8; 16]);
        auth_data.extend_from_slice(
            &u16::try_from(self.credential_id.len())
                .expect("credential id fits")
                .to_be_bytes(),
        );
        auth_data.extend_from_slice(&self.credential_id);
        auth_data.extend_from_slice(&self.cose_key());
        post_json(
            app,
            "/api/passkey/register/finish",
            &json!({
                "handle": handle,
                "credential": {
                    "id": b64(&self.credential_id),
                    "rawId": b64(&self.credential_id),
                    "type": "public-key",
                    "response": {
                        "clientDataJSON": b64(client_data_json.as_bytes()),
                        "attestationObject": b64(&attestation_object(&auth_data)),
                    },
                    "clientExtensionResults": {},
                },
            }),
        )
        .await
    }

    fn cose_key(&self) -> Vec<u8> {
        let point = self.key.verifying_key().to_sec1_point(false);
        let mut out = Vec::new();
        cbor_map(&mut out, 5);
        out.extend_from_slice(&[0x01, 0x02]);
        out.extend_from_slice(&[0x03, 0x26]);
        out.extend_from_slice(&[0x20, 0x01]);
        out.push(0x21);
        cbor_bytes(&mut out, point.x().expect("uncompressed P-256 point"));
        out.push(0x22);
        cbor_bytes(&mut out, point.y().expect("uncompressed P-256 point"));
        out
    }

    fn assertion(&self, challenge: &Challenge, flags: u8) -> Value {
        let client_data_json = format!(
            "{{\"type\":\"webauthn.get\",\"challenge\":\"{}\",\"origin\":\"{ORIGIN}\",\"crossOrigin\":false}}",
            b64(challenge.as_bytes())
        );
        let mut authenticator_data = Vec::new();
        authenticator_data.extend_from_slice(&Sha256::digest(RP_ID.as_bytes()));
        authenticator_data.push(flags);
        authenticator_data.extend_from_slice(&0u32.to_be_bytes());
        let mut message = authenticator_data.clone();
        message.extend_from_slice(&Sha256::digest(client_data_json.as_bytes()));
        let signature: Signature = self.key.sign(&message);
        json!({
            "credential_id": b64(&self.credential_id),
            "authenticator_data": b64(&authenticator_data),
            "client_data_json": b64(client_data_json.as_bytes()),
            "signature": b64(signature.to_der().as_bytes()),
        })
    }
}

fn cbor_map(out: &mut Vec<u8>, entries: u8) {
    out.push(0xa0 | entries);
}

fn cbor_text(out: &mut Vec<u8>, text: &str) {
    let len = u8::try_from(text.len()).expect("short CBOR text");
    assert!(len < 24, "CBOR text uses the short form");
    out.push(0x60 | len);
    out.extend_from_slice(text.as_bytes());
}

fn cbor_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u16::try_from(bytes.len()).expect("short CBOR byte string");
    if len < 24 {
        out.push(0x40 | len as u8);
    } else if len < 0x100 {
        out.push(0x58);
        out.push(len as u8);
    } else {
        out.push(0x59);
        out.extend_from_slice(&len.to_be_bytes());
    }
    out.extend_from_slice(bytes);
}

fn attestation_object(auth_data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    cbor_map(&mut out, 3);
    cbor_text(&mut out, "fmt");
    cbor_text(&mut out, "none");
    cbor_text(&mut out, "attStmt");
    cbor_map(&mut out, 0);
    cbor_text(&mut out, "authData");
    cbor_bytes(&mut out, auth_data);
    out
}

fn synced_session_request(
    nonce: &Nonce,
    challenge: &Challenge,
    assertion: Value,
    pow: bool,
) -> Value {
    let mut body = assertion;
    body["nonce"] = json!(b64(nonce.as_bytes()));
    if pow {
        body["pow"] = json!(
            solve(challenge, 1, &pow_params(), 0)
                .expect("pow")
                .as_array()
        );
    }
    body
}

#[tokio::test(flavor = "multi_thread")]
async fn a_synced_zero_counter_profile_completes_registration_and_session() {
    let server = TestServer::start().await;
    let passkey = SyncedPasskey::new(b"synced-profile");
    passkey.register(&server.app, "Jake!", 0x5d).await;

    let nonce = server.nonce().await;
    let challenge = Challenge::new(&nonce, &request_digest(RequestDomain::Session, ""));
    let request = synced_session_request(
        &nonce,
        &challenge,
        passkey.assertion(&challenge, 0x1d),
        true,
    );
    let (status, body) = post_json(&server.app, "/api/session", &request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["display_handle"], "Jake!");
    assert_eq!(body["tier"], "new");
    assert_eq!(body["roles"], json!([]));
    let token = body["status_token"]
        .as_str()
        .expect("status token")
        .to_owned();

    let nonce = server.nonce().await;
    let challenge = Challenge::new(&nonce, &request_digest(RequestDomain::Session, ""));
    let request = synced_session_request(
        &nonce,
        &challenge,
        passkey.assertion(&challenge, 0x1d),
        false,
    );
    let (status, body) = post_json(&server.app, "/api/session", &request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["display_handle"], "Jake!");

    let nonce = server.nonce().await;
    let challenge = Challenge::new(&nonce, &request_digest(RequestDomain::SessionRevoke, ""));
    let request = synced_session_request(
        &nonce,
        &challenge,
        passkey.assertion(&challenge, 0x1d),
        false,
    );
    let (status, body) = post_json(&server.app, "/api/session/revoke", &request).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let restore = Request::builder()
        .uri("/api/session")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("restore request");
    let (status, _, body) = send(&server.app, restore).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let error: Value = serde_json::from_slice(&body).expect("error response");
    assert_eq!(error["error"], "TOKEN_UNKNOWN");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_credential_id_an_assertion_cannot_carry_is_rejected_at_registration() {
    let server = TestServer::start().await;
    let empty = SyncedPasskey::new(b"");
    let (status, body) = empty.register_response(&server.app, "Jake!", 0x5d).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "MALFORMED_ENVELOPE");

    let longest = SyncedPasskey::new(&[3u8; looklab_upload::webauthn::MAX_CREDENTIAL_ID]);
    let (status, body) = longest.register_response(&server.app, "Jake!", 0x5d).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let nonce = server.nonce().await;
    let challenge = Challenge::new(&nonce, &request_digest(RequestDomain::Session, ""));
    let request = synced_session_request(
        &nonce,
        &challenge,
        longest.assertion(&challenge, 0x1d),
        true,
    );
    let (status, body) = post_json(&server.app, "/api/session", &request).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let longer = SyncedPasskey::new(&[4u8; looklab_upload::webauthn::MAX_CREDENTIAL_ID + 1]);
    let (status, body) = longer.register_response(&server.app, "Jake!", 0x5d).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "MALFORMED_ENVELOPE");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_assertion_without_user_verification_is_rejected() {
    let server = TestServer::start().await;
    let passkey = SyncedPasskey::new(b"unverified-profile");
    passkey.register(&server.app, "Jake!", 0x5d).await;

    let nonce = server.nonce().await;
    let challenge = Challenge::new(&nonce, &request_digest(RequestDomain::Session, ""));
    let request = synced_session_request(
        &nonce,
        &challenge,
        passkey.assertion(&challenge, 0x19),
        true,
    );
    let (status, body) = post_json(&server.app, "/api/session", &request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"], "SIGNATURE_INVALID");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_assertion_with_backup_state_but_no_eligibility_is_rejected() {
    let server = TestServer::start().await;
    let passkey = SyncedPasskey::new(b"device-bound-profile");
    passkey.register(&server.app, "Jake!", 0x45).await;

    let nonce = server.nonce().await;
    let challenge = Challenge::new(&nonce, &request_digest(RequestDomain::Session, ""));
    let request = synced_session_request(
        &nonce,
        &challenge,
        passkey.assertion(&challenge, 0x15),
        true,
    );
    let (status, body) = post_json(&server.app, "/api/session", &request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"], "SIGNATURE_INVALID");
}

fn documented_camera(id: &str) -> ManualCamera {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../data/manuals")
        .join(format!("{}.yaml", id.replace('/', "-")));
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let catalog = validate_manual_catalog(&source).expect("manual catalog");
    catalog
        .camera(&BodyId::new(id).expect("body id"))
        .cloned()
        .expect("documented camera")
}

fn confirmation_for(camera: &ManualCamera, key: &str, firmware: &str) -> ManualConfirmation {
    confirmation_with(camera, key, firmware, Finding::Matches)
}

fn confirmation_with(
    camera: &ManualCamera,
    key: &str,
    firmware: &str,
    finding: Finding,
) -> ManualConfirmation {
    let capability = camera
        .capabilities
        .iter()
        .find(|capability| capability.key.as_str() == key)
        .unwrap_or_else(|| panic!("control {key}"));
    ManualConfirmation {
        version: looklab_schema::MANUAL_CONFIRMATION_VERSION,
        camera: camera.id.clone(),
        camera_digest: camera.definition_digest(),
        firmware: ReportedFirmware::new(firmware).expect("firmware"),
        scope: ConfirmationScope::Controls {
            keys: vec![capability.key],
        },
        observations: vec![ControlObservation {
            key: capability.key,
            definition_digest: capability.definition_digest(camera),
            finding,
        }],
    }
}

fn confirmation_request(
    nonce: &Nonce,
    send: &SendManifest,
    assertion: &WebAuthnAssertion,
    confirmation: &ManualConfirmation,
) -> ManualConfirmationRequest {
    ManualConfirmationRequest {
        nonce: b64(nonce.as_bytes()),
        credential_id: b64(&assertion.credential_id),
        authenticator_data: b64(&assertion.authenticator_data),
        client_data_json: b64(&assertion.client_data_json),
        signature: b64(&assertion.signature),
        send_manifest: b64(&canonical_send_manifest_bytes(send)),
        confirmation: confirmation.clone(),
    }
}

fn envelope_with_send(
    nonce: Nonce,
    send: &SendManifest,
    manifest: &BatchManifest,
    blobs: &[Vec<u8>],
    serial: Option<&RawSerial>,
    assertion: &WebAuthnAssertion,
) -> Vec<u8> {
    let pow = solve(
        &Challenge::new(&nonce, &manifest_hash(manifest)),
        u32::try_from(manifest.images.len()).expect("image count"),
        &pow_params(),
        0,
    )
    .expect("proof-of-work");
    encode_envelope(&EnvelopeParts {
        nonce,
        send_manifest: send,
        manifest,
        assertion,
        pow: &pow,
        serial,
        blobs,
    })
}

async fn post_standalone_confirmation(
    server: &mut TestServer,
    confirmation: &ManualConfirmation,
) -> StatusCode {
    let nonce = server.nonce().await;
    let digest = confirmation.digest();
    let send = SendManifest::canonical_with_attestations(
        Vec::new(),
        vec![AttestationHash::from_bytes(*digest.as_bytes())],
    )
    .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = server.assertion(&challenge).await;
    let request = confirmation_request(&nonce, &send, &assertion, confirmation);
    post_confirmation(&server.app, &request).await.0
}

async fn standalone_confirmation_request(
    server: &mut TestServer,
    confirmation: &ManualConfirmation,
) -> ManualConfirmationRequest {
    let nonce = server.nonce().await;
    let digest = confirmation.digest();
    let send = SendManifest::canonical_with_attestations(
        Vec::new(),
        vec![AttestationHash::from_bytes(*digest.as_bytes())],
    )
    .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = server.assertion(&challenge).await;
    confirmation_request(&nonce, &send, &assertion, confirmation)
}

async fn post_standalone_confirmation_response(
    server: &mut TestServer,
    confirmation: &ManualConfirmation,
) -> (StatusCode, Value) {
    let request = standalone_confirmation_request(server, confirmation).await;
    post_confirmation(&server.app, &request).await
}

async fn synced_confirmation_request(
    server: &TestServer,
    passkey: &SyncedPasskey,
    confirmation: &ManualConfirmation,
) -> ManualConfirmationRequest {
    let nonce = server.nonce().await;
    let digest = confirmation.digest();
    let send = SendManifest::canonical_with_attestations(
        Vec::new(),
        vec![AttestationHash::from_bytes(*digest.as_bytes())],
    )
    .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = passkey.assertion(&challenge, 0x1d);
    ManualConfirmationRequest {
        nonce: b64(nonce.as_bytes()),
        credential_id: assertion["credential_id"]
            .as_str()
            .expect("credential id")
            .to_owned(),
        authenticator_data: assertion["authenticator_data"]
            .as_str()
            .expect("authenticator data")
            .to_owned(),
        client_data_json: assertion["client_data_json"]
            .as_str()
            .expect("client data")
            .to_owned(),
        signature: assertion["signature"]
            .as_str()
            .expect("signature")
            .to_owned(),
        send_manifest: b64(&canonical_send_manifest_bytes(&send)),
        confirmation: confirmation.clone(),
    }
}

async fn synced_sign_in(server: &TestServer, passkey: &SyncedPasskey) {
    let nonce = server.nonce().await;
    let challenge = Challenge::new(&nonce, &request_digest(RequestDomain::Session, ""));
    let request = synced_session_request(
        &nonce,
        &challenge,
        passkey.assertion(&challenge, 0x1d),
        true,
    );
    let (status, body) = post_json(&server.app, "/api/session", &request).await;
    assert_eq!(status, StatusCode::OK, "session: {body}");
}

async fn synced_owner(
    server: &TestServer,
    credential_id: &[u8],
    display_handle: &str,
) -> (SyncedPasskey, String) {
    let passkey = SyncedPasskey::new(credential_id);
    passkey.register(&server.app, display_handle, 0x5d).await;
    synced_sign_in(server, &passkey).await;
    let uuid = server.credential_uuid_for(credential_id).await;
    (passkey, uuid)
}

async fn insert_serial_contribution(db: &SqlitePool, id: &str, credential: &str, hmac: [u8; 32]) {
    sqlx::query(
        "INSERT INTO contributions
         (id, credential_id, batch_manifest_hash, signature, pow, kind, serial_hmac, intent,
          body_id, firmware_version, sim_key, settings, status, created_at)
         VALUES (?, ?, x'00', x'00', 'pow', 'chart', ?, 'native',
                 'fujifilm/x-t5', '2.10', 'classic-chrome', '{}', 'pending', 0)",
    )
    .bind(id)
    .bind(credential)
    .bind(hmac.to_vec())
    .execute(db)
    .await
    .expect("contribution");
}

async fn insert_confirmation(
    db: &SqlitePool,
    credential: &str,
    camera: &ManualCamera,
    key: &str,
    finding: &str,
    payload: u8,
    created_at: i64,
) {
    insert_confirmation_with_firmware(
        db,
        credential,
        camera,
        key,
        &ReportedFirmware::new("2.10").expect("firmware"),
        finding,
        ConfirmationOccurrence {
            payload,
            created_at,
        },
    )
    .await;
}

struct ConfirmationOccurrence {
    payload: u8,
    created_at: i64,
}

async fn insert_confirmation_with_firmware(
    db: &SqlitePool,
    credential: &str,
    camera: &ManualCamera,
    key: &str,
    firmware: &ReportedFirmware,
    finding: &str,
    occurrence: ConfirmationOccurrence,
) {
    let ConfirmationOccurrence {
        payload,
        created_at,
    } = occurrence;
    let capability = camera
        .capabilities
        .iter()
        .find(|capability| capability.key.as_str() == key)
        .expect("control");
    let result = sqlx::query(
        "INSERT INTO manual_confirmations
         (credential_id, camera_id, camera_digest, firmware, scope_kind, scope_json,
          payload_digest, assertion_hash, send_nonce, created_at)
         VALUES (?, ?, ?, ?, 'controls', '{}', ?, x'00', NULL, ?)",
    )
    .bind(credential)
    .bind(camera.id.as_str())
    .bind(camera.definition_digest().as_bytes().as_slice())
    .bind(firmware.as_str())
    .bind([payload; 32].to_vec())
    .bind(created_at)
    .execute(db)
    .await
    .expect("confirmation");
    sqlx::query(
        "INSERT INTO manual_observations
         (confirmation_id, capability_key, definition_digest, finding_json)
         VALUES (?, ?, ?, ?)",
    )
    .bind(result.last_insert_rowid())
    .bind(key)
    .bind(capability.definition_digest(camera).as_bytes().as_slice())
    .bind(finding)
    .execute(db)
    .await
    .expect("observation");
    sqlx::query(
        "INSERT INTO manual_confirmation_occurrences (confirmation_id, send_nonce, received_at)
         VALUES (?, ?, ?)",
    )
    .bind(result.last_insert_rowid())
    .bind(format!("seed-{payload:02x}"))
    .bind(created_at)
    .execute(db)
    .await
    .expect("occurrence");
}

fn summary_control<'a>(summary: &'a Value, key: &str) -> &'a Value {
    summary["controls"]
        .as_array()
        .expect("controls")
        .iter()
        .find(|control| control["key"] == key)
        .unwrap_or_else(|| panic!("control {key}"))
}

async fn manual_summary(server: &TestServer, make: &str, model: &str) -> Value {
    let camera = BodyId::new(&format!(
        "{}/{}",
        make.to_ascii_lowercase(),
        model.to_ascii_lowercase().replace(' ', "-")
    ))
    .expect("camera id");
    let (status, _, body) = server
        .get(&format!("/api/manual-confirmations/{}", camera.as_str()))
        .await;
    assert_eq!(status, StatusCode::OK, "summary: {body:?}");
    serde_json::from_slice(&body).expect("summary json")
}

async fn moderation_queue(server: &TestServer, token: &str) -> Value {
    let request = Request::builder()
        .method("GET")
        .uri("/api/mod/queue")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("queue request");
    let (status, _, body) = send(&server.app, request).await;
    assert_eq!(status, StatusCode::OK, "queue: {body:?}");
    serde_json::from_slice(&body).expect("queue json")
}

async fn post_json_with_token(
    app: &Router,
    path: &str,
    token: &str,
    value: &Value,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(value).expect("json")))
        .expect("request");
    let (status, _, body) = send(app, request).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

async fn post_signed_with_token(
    server: &mut TestServer,
    path: &str,
    token: &str,
    domain: &str,
    payload: &[u8],
    mut body: Value,
) -> (StatusCode, Value) {
    let signed = server.signed_fields(domain, payload).await;
    for (key, value) in signed.as_object().expect("signed object") {
        body[key.as_str()] = value.clone();
    }
    post_json_with_token(&server.app, path, token, &body).await
}

#[tokio::test]
async fn manual_routes_lookup_catalog_camera_ids() {
    let server = TestServer::start().await;
    let (status, _, bytes) = server.get("/api/manuals/canon/eos-r5").await;
    assert_eq!(status, StatusCode::OK);
    let page: looklab_publish::ManualPage = serde_json::from_slice(&bytes).expect("manual page");
    assert_eq!(page.id.as_str(), "canon/eos-r5");
    assert!(!page.preparation.is_ready());
    let (status, _, bytes) = server.get("/api/manual-confirmations/canon/eos-r5").await;
    assert_eq!(status, StatusCode::OK);
    let summary: looklab_schema::ManualConfirmationSummary =
        serde_json::from_slice(&bytes).expect("manual summary");
    assert!(!summary.human_confirmed());
}

#[tokio::test]
async fn manual_confirmation_after_photos_reuses_the_send_assertion() {
    let mut server = TestServer::start().await;
    let camera = documented_camera("fujifilm/x-t5");
    let confirmation = confirmation_for(&camera, "native_simulations", "2.10");
    let digest = confirmation.digest();
    let (manifest, blobs) = manifest(valid_settings());
    let nonce = server.nonce().await;
    let send = SendManifest::canonical_with_attestations(
        vec![manifest_hash(&manifest)],
        vec![AttestationHash::from_bytes(*digest.as_bytes())],
    )
    .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = server.assertion(&challenge).await;
    let serial = RawSerial::from_envelope_ascii(SERIAL.as_bytes()).expect("serial");
    let (status, body) = server
        .upload_bytes(envelope_with_send(
            nonce,
            &send,
            &manifest,
            &blobs,
            Some(&serial),
            &assertion,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");

    let request = confirmation_request(&nonce, &send, &assertion, &confirmation);
    let (status, summary) = post_confirmation(&server.app, &request).await;
    assert_eq!(status, StatusCode::OK, "confirmation: {summary}");
    let control = summary_control(&summary, "native_simulations");
    assert_eq!(control["state"], "owner_reported");
    assert_eq!(control["agreeing_groups"], 1);
    assert_eq!(control["firmware"][0]["reported_profiles"], 1);

    let hmac = serial.hmac(&server.state.secret.hmac_key(), "Fujifilm", "X-T5");
    let summary_text = serde_json::to_string(&summary).expect("summary text");
    assert!(!summary_text.contains(SERIAL));
    assert!(!summary_text.contains(&hex(hmac.as_bytes())));

    let (status, _) = post_confirmation(&server.app, &request).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let contribution = body["contribution_id"]
        .as_str()
        .expect("contribution id")
        .to_owned();
    let mut withdraw = server
        .signed_fields("withdraw", contribution.as_bytes())
        .await;
    withdraw["contribution_ids"] = json!([contribution]);
    let (status, response) = post_json(&server.app, "/api/withdraw", &withdraw).await;
    assert_eq!(status, StatusCode::OK, "withdraw: {response}");
    let withdrawn: Option<i64> =
        sqlx::query_scalar("SELECT withdrawn_at FROM manual_confirmations LIMIT 1")
            .fetch_one(&server.db)
            .await
            .expect("withdrawn");
    assert!(withdrawn.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_upload_and_manual_attestation_under_one_send_both_succeed() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let confirmation = confirmation_for(&camera, "native_simulations", "2.10");
    let digest = confirmation.digest();
    let (manifest, blobs) = manifest(valid_settings());
    let nonce = server.nonce().await;
    let send = SendManifest::canonical_with_attestations(
        vec![manifest_hash(&manifest)],
        vec![AttestationHash::from_bytes(*digest.as_bytes())],
    )
    .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = server.assertion(&challenge).await;
    let serial = RawSerial::from_envelope_ascii(SERIAL.as_bytes()).expect("serial");
    let upload = envelope_with_send(nonce, &send, &manifest, &blobs, Some(&serial), &assertion);
    let request = confirmation_request(&nonce, &send, &assertion, &confirmation);
    let (upload_result, confirmation_result) = tokio::join!(
        server.upload_bytes(upload),
        post_confirmation(&server.app, &request),
    );
    assert_eq!(upload_result.0, StatusCode::CREATED);
    assert_eq!(confirmation_result.0, StatusCode::OK);
    let contributions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM contributions WHERE batch_manifest_hash = ?")
            .bind(manifest_hash(&manifest).as_bytes().as_slice())
            .fetch_one(&server.db)
            .await
            .expect("contributions");
    assert_eq!(contributions, 1);
    let confirmations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM manual_confirmations")
        .fetch_one(&server.db)
        .await
        .expect("confirmations");
    assert_eq!(confirmations, 1);

    let other_nonce = server.nonce().await;
    let other = send_envelope(other_nonce, &send, &manifest, &blobs, &assertion);
    let (status, body) = server.upload_bytes(other).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "SIGNATURE_INVALID");
}

#[tokio::test]
async fn withdrawn_send_rejects_a_late_confirmation() {
    let mut server = TestServer::start().await;
    let camera = documented_camera("fujifilm/x-t5");
    let confirmation = confirmation_for(&camera, "native_simulations", "2.10");
    let digest = confirmation.digest();
    let (manifest, blobs) = manifest(valid_settings());
    let nonce = server.nonce().await;
    let send = SendManifest::canonical_with_attestations(
        vec![manifest_hash(&manifest)],
        vec![AttestationHash::from_bytes(*digest.as_bytes())],
    )
    .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = server.assertion(&challenge).await;
    let serial = RawSerial::from_envelope_ascii(SERIAL.as_bytes()).expect("serial");
    let (status, body) = server
        .upload_bytes(envelope_with_send(
            nonce,
            &send,
            &manifest,
            &blobs,
            Some(&serial),
            &assertion,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
    let contribution = body["contribution_id"]
        .as_str()
        .expect("contribution id")
        .to_owned();
    let mut withdraw = server
        .signed_fields("withdraw", contribution.as_bytes())
        .await;
    withdraw["contribution_ids"] = json!([contribution]);
    let (status, response) = post_json(&server.app, "/api/withdraw", &withdraw).await;
    assert_eq!(status, StatusCode::OK, "withdraw: {response}");

    let (status, _) = post_confirmation(
        &server.app,
        &confirmation_request(&nonce, &send, &assertion, &confirmation),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn standalone_attestation_only_send_is_valid_and_replays_rejected() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let confirmation = confirmation_for(&camera, "native_simulations", "2.10");
    let digest = confirmation.digest();
    let nonce = server.nonce().await;
    let send = SendManifest::canonical_with_attestations(
        Vec::new(),
        vec![AttestationHash::from_bytes(*digest.as_bytes())],
    )
    .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = server.assertion(&challenge).await;
    let request = confirmation_request(&nonce, &send, &assertion, &confirmation);
    let (status, summary) = post_confirmation(&server.app, &request).await;
    assert_eq!(status, StatusCode::OK, "confirmation: {summary}");
    assert_eq!(
        summary_control(&summary, "native_simulations")["agreeing_groups"],
        0
    );

    let (status, _) = post_confirmation(&server.app, &request).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn standalone_confirmation_reports_unlinked_intake_and_queue() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let confirmation = confirmation_for(&camera, "native_simulations", "2.10");

    let (status, submitted) =
        post_standalone_confirmation_response(&mut server, &confirmation).await;
    assert_eq!(status, StatusCode::OK, "confirmation: {submitted}");
    assert_eq!(submitted["receipt"]["camera"], "fujifilm/x-t5");
    assert_eq!(submitted["receipt"]["owner_linkage"], "unlinked");
    assert_eq!(submitted["receipt"]["recorded_checks"], 1);

    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    assert!(summary.get("receipt").is_none());
    let control = summary_control(&summary, "native_simulations");
    assert_eq!(control["intake"]["recorded"], 1);
    assert_eq!(control["intake"]["unlinked"], 1);
    assert_eq!(control["state"], "owner_reported");
    assert_eq!(control["agreeing_groups"], 0);
    assert_eq!(control["firmware"][0]["owner_profiles"], 1);
    assert_eq!(summary["owner_reported"], 1);

    let (status, _, body) = server.get("/api/manuals/fujifilm/x-t5").await;
    assert_eq!(status, StatusCode::OK);
    let page: Value = serde_json::from_slice(&body).expect("manual page");
    assert!(page.get("receipt").is_none());
    let page_control = summary_control(&page["confirmation"], "native_simulations");
    assert_eq!(page_control["intake"]["recorded"], 1);
    assert_eq!(page_control["intake"]["unlinked"], 1);
    assert!(page_control.get("receipt").is_none());

    let token = server.moderator_token().await;
    let queue = moderation_queue(&server, &token).await;
    let checks = queue["manual_checks"].as_array().expect("manual checks");
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0]["camera"], "fujifilm/x-t5");
    assert_eq!(checks[0]["control"], "native_simulations");
    assert_eq!(checks[0]["finding"]["kind"], "matches");
    assert_eq!(checks[0]["status"], "counted");
    assert_eq!(checks[0]["owner_linkage"], "unlinked");
    assert!(
        queue["manual_conflicts"]
            .as_array()
            .expect("conflicts")
            .is_empty()
    );

    let (status, resent) = post_standalone_confirmation_response(&mut server, &confirmation).await;
    assert_eq!(status, StatusCode::OK, "resend: {resent}");
    let queue = moderation_queue(&server, &token).await;
    assert_eq!(
        queue["manual_checks"]
            .as_array()
            .expect("manual checks")
            .len(),
        1
    );
}

#[tokio::test]
async fn two_unlinked_profiles_report_owner_reported_without_a_conflict() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let confirmation = confirmation_for(&camera, "native_simulations", "2.10");
    let (status, first) = post_standalone_confirmation_response(&mut server, &confirmation).await;
    assert_eq!(status, StatusCode::OK, "first: {first}");
    assert_eq!(first["receipt"]["owner_linkage"], "unlinked");

    let (owner_two, _) = synced_owner(&server, b"owner-reported-two", "Owner Two").await;
    let request = synced_confirmation_request(&server, &owner_two, &confirmation).await;
    let (status, second) = post_confirmation(&server.app, &request).await;
    assert_eq!(status, StatusCode::OK, "second: {second}");
    assert_eq!(second["receipt"]["owner_linkage"], "unlinked");

    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    let control = summary_control(&summary, "native_simulations");
    assert_eq!(control["state"], "owner_reported");
    assert_eq!(control["agreeing_groups"], 0);
    assert_eq!(control["firmware"][0]["owner_profiles"], 2);
    assert_eq!(
        control["firmware"][0]["reported_options"][0]["owner_profiles"],
        2
    );
    assert_eq!(summary["confirmed"], 0);
    assert_eq!(summary["owner_reported"], 1);

    let token = server.moderator_token().await;
    let queue = moderation_queue(&server, &token).await;
    let checks = queue["manual_checks"].as_array().expect("manual checks");
    assert_eq!(checks.len(), 2);
    assert!(
        checks
            .iter()
            .all(|check| { check["status"] == "counted" && check["owner_linkage"] == "unlinked" })
    );
    assert!(
        queue["manual_conflicts"]
            .as_array()
            .expect("conflicts")
            .is_empty()
    );

    let promotions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE type = 'manual_confirmation_promotion'",
    )
    .fetch_one(&server.db)
    .await
    .expect("promotion rows");
    assert_eq!(promotions, 0);
}

#[tokio::test]
async fn unlinked_disagreement_creates_one_conflict_with_every_option() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let matching = confirmation_for(&camera, "native_simulations", "2.10");
    let (status, first) = post_standalone_confirmation_response(&mut server, &matching).await;
    assert_eq!(status, StatusCode::OK, "first: {first}");

    let (owner_two, _) = synced_owner(&server, b"disagree-two", "Owner Two").await;
    let request = synced_confirmation_request(&server, &owner_two, &matching).await;
    let (status, second) = post_confirmation(&server.app, &request).await;
    assert_eq!(status, StatusCode::OK, "second: {second}");

    let corrected = confirmation_with(
        &camera,
        "native_simulations",
        "2.10",
        Finding::Corrected {
            availability: looklab_schema::Availability::Unavailable,
            domain: looklab_schema::CapabilityDomain::Unspecified,
        },
    );
    let (owner_three, _) = synced_owner(&server, b"disagree-three", "Owner Three").await;
    let request = synced_confirmation_request(&server, &owner_three, &corrected).await;
    let (status, third) = post_confirmation(&server.app, &request).await;
    assert_eq!(status, StatusCode::OK, "third: {third}");

    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    let control = summary_control(&summary, "native_simulations");
    assert_eq!(control["state"], "contradicted");
    let options = control["firmware"][0]["reported_options"]
        .as_array()
        .expect("options");
    assert_eq!(options.len(), 2);
    assert_eq!(options[0]["finding"]["kind"], "matches");
    assert_eq!(options[0]["owner_profiles"], 2);
    assert_eq!(options[1]["finding"]["kind"], "corrected");
    assert_eq!(options[1]["owner_profiles"], 1);

    let token = server.moderator_token().await;
    let queue = moderation_queue(&server, &token).await;
    let checks = queue["manual_checks"].as_array().expect("manual checks");
    assert_eq!(checks.len(), 3);
    assert!(checks.iter().all(|check| check["status"] == "conflicted"));
    let conflicts = queue["manual_conflicts"].as_array().expect("conflicts");
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0]["firmware"], "2.10");
    assert_eq!(
        conflicts[0]["options"].as_array().expect("options").len(),
        2
    );
}

#[tokio::test]
async fn moderator_decision_overrides_owner_reports_for_its_scope() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let matching_two = confirmation_for(&camera, "native_simulations", "2.10");
    let matching_three = confirmation_for(&camera, "native_simulations", "3.00");
    let corrected = |firmware: &str| {
        confirmation_with(
            &camera,
            "native_simulations",
            firmware,
            Finding::Corrected {
                availability: looklab_schema::Availability::Unavailable,
                domain: looklab_schema::CapabilityDomain::Unspecified,
            },
        )
    };
    for (label, confirmation) in [
        ("two-match", matching_two.clone()),
        ("two-corrected", corrected("2.10")),
        ("three-match", matching_three.clone()),
        ("three-corrected", corrected("3.00")),
    ] {
        let (owner, _) = synced_owner(&server, label.as_bytes(), label).await;
        let request = synced_confirmation_request(&server, &owner, &confirmation).await;
        let (status, body) = post_confirmation(&server.app, &request).await;
        assert_eq!(status, StatusCode::OK, "{label}: {body}");
    }

    let token = server.moderator_token().await;
    let key = CapabilityKey::NativeSimulations;
    let definition_digest = camera
        .capabilities
        .iter()
        .find(|capability| capability.key == key)
        .expect("control")
        .definition_digest(&camera);
    let moderation = ManualModeration {
        camera: camera.id.clone(),
        key,
        definition_digest,
        confirmed: true,
        evidence: ModeratorEvidence {
            kind: ModeratorEvidenceKind::Physical,
            url: None,
            description: "body review".to_owned(),
            firmware_scope: ModeratorFirmwareScope::exact("2.10").expect("scope"),
        },
    };
    let body = json!({
        "definition_digest": definition_digest,
        "confirmed": true,
        "evidence": {
            "kind": "physical",
            "description": "body review",
            "firmware_scope": {"kind": "exact", "firmware": "2.10"},
        },
    });
    let path = "/api/manual-confirmations/fujifilm/x-t5/native_simulations/moderate";
    let (status, summary) = post_signed_with_token(
        &mut server,
        path,
        &token,
        "moderate-manual-confirmation",
        &moderation.canonical_bytes(),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "moderate: {summary}");

    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    let control = summary_control(&summary, "native_simulations");
    let two = control["firmware"]
        .as_array()
        .expect("firmware")
        .iter()
        .find(|result| result["firmware"] == "2.10")
        .expect("2.10 result");
    let three = control["firmware"]
        .as_array()
        .expect("firmware")
        .iter()
        .find(|result| result["firmware"] == "3.00")
        .expect("3.00 result");
    assert_eq!(two["state"], "moderator_confirmed");
    assert_eq!(three["state"], "contradicted");

    let queue = moderation_queue(&server, &token).await;
    let conflicts = queue["manual_conflicts"].as_array().expect("conflicts");
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0]["firmware"], "3.00");
}

#[tokio::test]
async fn conflict_needs_a_matching_moderator_scope() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let matching = confirmation_for(&camera, "native_simulations", "2.10");
    assert_eq!(
        post_standalone_confirmation(&mut server, &matching).await,
        StatusCode::OK
    );
    let corrected = confirmation_with(
        &camera,
        "native_simulations",
        "2.10",
        Finding::Corrected {
            availability: looklab_schema::Availability::Unavailable,
            domain: looklab_schema::CapabilityDomain::Unspecified,
        },
    );
    let (owner_two, _) = synced_owner(&server, b"scope-conflict", "Scope Conflict").await;
    let request = synced_confirmation_request(&server, &owner_two, &corrected).await;
    let (status, body) = post_confirmation(&server.app, &request).await;
    assert_eq!(status, StatusCode::OK, "second: {body}");

    let token = server.moderator_token().await;
    let key = CapabilityKey::NativeSimulations;
    let definition_digest = camera
        .capabilities
        .iter()
        .find(|capability| capability.key == key)
        .expect("control")
        .definition_digest(&camera);
    let path = "/api/manual-confirmations/fujifilm/x-t5/native_simulations/moderate";
    for (scope, expected, expected_state) in [
        (ModeratorFirmwareScope::unspecified(), 1, "contradicted"),
        (
            ModeratorFirmwareScope::exact("3.00").expect("scope"),
            1,
            "contradicted",
        ),
        (
            ModeratorFirmwareScope::exact("2.10").expect("scope"),
            0,
            "moderator_confirmed",
        ),
    ] {
        let moderation = ManualModeration {
            camera: camera.id.clone(),
            key,
            definition_digest,
            confirmed: true,
            evidence: ModeratorEvidence {
                kind: ModeratorEvidenceKind::Physical,
                url: None,
                description: "body review".to_owned(),
                firmware_scope: scope.clone(),
            },
        };
        let body = json!({
            "definition_digest": definition_digest,
            "confirmed": true,
            "evidence": {
                "kind": "physical",
                "description": "body review",
                "firmware_scope": serde_json::to_value(&scope).expect("scope json"),
            },
        });
        let (status, summary) = post_signed_with_token(
            &mut server,
            path,
            &token,
            "moderate-manual-confirmation",
            &moderation.canonical_bytes(),
            body,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "moderate: {summary}");
        assert_eq!(
            summary_control(&summary, "native_simulations")["state"],
            expected_state
        );
        let queue = moderation_queue(&server, &token).await;
        assert_eq!(
            queue["manual_conflicts"]
                .as_array()
                .expect("conflicts")
                .len(),
            expected
        );
    }
}

#[tokio::test]
async fn serial_photo_moves_the_newest_answer_into_the_linked_count() {
    let mut server = TestServer::start().await;
    let owner_one = server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let matching = confirmation_for(&camera, "native_simulations", "2.10");
    let (status, submitted) = post_standalone_confirmation_response(&mut server, &matching).await;
    assert_eq!(status, StatusCode::OK, "first: {submitted}");
    assert_eq!(submitted["receipt"]["owner_linkage"], "unlinked");
    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    assert_eq!(
        summary_control(&summary, "native_simulations")["state"],
        "owner_reported"
    );

    insert_serial_contribution(&server.db, "contrib-linked-one", &owner_one, [11; 32]).await;
    let moved = manual_summary(&server, "Fujifilm", "X-T5").await;
    let control = summary_control(&moved, "native_simulations");
    assert_eq!(control["state"], "owner_reported");
    assert_eq!(control["agreeing_groups"], 1);
    let result = &control["firmware"][0];
    assert_eq!(result["owner_profiles"], 0);
    assert_eq!(result["reported_profiles"], 1);
    let options = result["reported_options"].as_array().expect("options");
    assert_eq!(options.len(), 1, "the answer must not be counted twice");
    assert_eq!(options[0]["finding"]["kind"], "matches");
    assert_eq!(options[0]["linked_groups"], 1);
    assert_eq!(options[0]["owner_profiles"], 0);

    let corrected = confirmation_with(
        &camera,
        "native_simulations",
        "2.10",
        Finding::Corrected {
            availability: looklab_schema::Availability::Unavailable,
            domain: looklab_schema::CapabilityDomain::Unspecified,
        },
    );
    let (status, submitted) = post_standalone_confirmation_response(&mut server, &corrected).await;
    assert_eq!(status, StatusCode::OK, "linked: {submitted}");
    assert_eq!(submitted["receipt"]["owner_linkage"], "linked");

    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    let control = summary_control(&summary, "native_simulations");
    assert_eq!(control["state"], "owner_reported");
    assert_eq!(control["agreeing_groups"], 1);
    let result = &control["firmware"][0];
    assert_eq!(result["owner_profiles"], 0);
    assert_eq!(result["reported_profiles"], 1);
    let options = result["reported_options"].as_array().expect("options");
    assert_eq!(options.len(), 1);
    assert_eq!(options[0]["finding"]["kind"], "corrected");
    assert_eq!(options[0]["linked_groups"], 1);
    assert_eq!(options[0]["owner_profiles"], 0);

    let promotions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE type = 'manual_confirmation_promotion'",
    )
    .fetch_one(&server.db)
    .await
    .expect("promotion rows");
    assert_eq!(promotions, 0);

    for (label, serial) in [("linked-two", [12; 32]), ("linked-three", [13; 32])] {
        let (owner, owner_id) = synced_owner(&server, label.as_bytes(), label).await;
        insert_serial_contribution(&server.db, &format!("contrib-{label}"), &owner_id, serial)
            .await;
        let request = synced_confirmation_request(&server, &owner, &corrected).await;
        let (status, body) = post_confirmation(&server.app, &request).await;
        assert_eq!(status, StatusCode::OK, "{label}: {body}");
    }

    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    let control = summary_control(&summary, "native_simulations");
    assert_eq!(control["state"], "confirmed");
    assert_eq!(control["agreeing_groups"], 3);
    let promotions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE type = 'manual_confirmation_promotion'",
    )
    .fetch_one(&server.db)
    .await
    .expect("promotion rows");
    assert_eq!(promotions, 1);
}

#[tokio::test]
async fn stale_definition_check_is_counted_as_stale_intake() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let confirmation = confirmation_for(&camera, "native_simulations", "2.10");
    let (status, _) = post_standalone_confirmation_response(&mut server, &confirmation).await;
    assert_eq!(status, StatusCode::OK);

    let credential = server.credential_uuid().await;
    sqlx::query(
        "UPDATE manual_observations SET definition_digest =
         x'0000000000000000000000000000000000000000000000000000000000000000'
         WHERE confirmation_id IN (SELECT id FROM manual_confirmations
                                   WHERE credential_id = ? AND camera_id = ?)",
    )
    .bind(credential.as_str())
    .bind(camera.id.as_str())
    .execute(&server.db)
    .await
    .expect("stale digest");

    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    let control = summary_control(&summary, "native_simulations");
    assert_eq!(control["intake"]["recorded"], 0);
    assert_eq!(control["intake"]["stale"], 1);
    assert_eq!(control["agreeing_groups"], 0);

    let token = server.moderator_token().await;
    let queue = moderation_queue(&server, &token).await;
    let checks = queue["manual_checks"].as_array().expect("manual checks");
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0]["status"], "stale_definition");
}

#[tokio::test]
async fn moderator_watermark_separates_later_same_second_checks() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let earlier = confirmation_for(&camera, "native_simulations", "2.10");
    assert_eq!(
        post_standalone_confirmation(&mut server, &earlier).await,
        StatusCode::OK
    );

    let token = server.moderator_token().await;
    let key = CapabilityKey::NativeSimulations;
    let definition_digest = camera
        .capabilities
        .iter()
        .find(|capability| capability.key == key)
        .expect("control")
        .definition_digest(&camera);
    let moderation = ManualModeration {
        camera: camera.id.clone(),
        key,
        definition_digest,
        confirmed: true,
        evidence: ModeratorEvidence {
            kind: ModeratorEvidenceKind::Physical,
            url: None,
            description: "body review".to_owned(),
            firmware_scope: ModeratorFirmwareScope::exact("2.10").expect("scope"),
        },
    };
    let body = json!({
        "definition_digest": definition_digest,
        "confirmed": true,
        "evidence": {
            "kind": "physical",
            "description": "body review",
            "firmware_scope": {"kind": "exact", "firmware": "2.10"},
        },
    });
    let path = "/api/manual-confirmations/fujifilm/x-t5/native_simulations/moderate";
    let (status, summary) = post_signed_with_token(
        &mut server,
        path,
        &token,
        "moderate-manual-confirmation",
        &moderation.canonical_bytes(),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "moderate: {summary}");

    let decision_created: i64 = sqlx::query_scalar(
        "SELECT created_at FROM manual_moderator_confirmations
         WHERE camera_id = ? AND capability_key = 'native_simulations'
         ORDER BY id DESC LIMIT 1",
    )
    .bind(camera.id.as_str())
    .fetch_one(&server.db)
    .await
    .expect("moderator record");

    let (later_passkey, _) = synced_owner(&server, b"later-owner", "Later Owner").await;
    let later_confirmation = confirmation_with(
        &camera,
        "native_simulations",
        "2.10",
        Finding::Corrected {
            availability: looklab_schema::Availability::Unavailable,
            domain: looklab_schema::CapabilityDomain::Unspecified,
        },
    );
    let later = synced_confirmation_request(&server, &later_passkey, &later_confirmation).await;
    let (status, submitted) = post_confirmation(&server.app, &later).await;
    assert_eq!(status, StatusCode::OK, "later: {submitted}");

    let later_occurrence: i64 =
        sqlx::query_scalar("SELECT MAX(id) FROM manual_confirmation_occurrences")
            .fetch_one(&server.db)
            .await
            .expect("later occurrence");
    sqlx::query("UPDATE manual_confirmation_occurrences SET received_at = ? WHERE id = ?")
        .bind(decision_created)
        .bind(later_occurrence)
        .execute(&server.db)
        .await
        .expect("colliding timestamp");
    let later_received: i64 =
        sqlx::query_scalar("SELECT received_at FROM manual_confirmation_occurrences WHERE id = ?")
            .bind(later_occurrence)
            .fetch_one(&server.db)
            .await
            .expect("later timestamp");
    assert_eq!(later_received, decision_created);

    let queue = moderation_queue(&server, &token).await;
    let checks = queue["manual_checks"].as_array().expect("manual checks");
    assert_eq!(checks.len(), 2);
    let earlier = checks
        .iter()
        .find(|check| check["finding"]["kind"] == "matches")
        .expect("earlier matching check");
    let later = checks
        .iter()
        .find(|check| check["finding"]["kind"] == "corrected")
        .expect("later corrected check");
    assert_eq!(earlier["status"], "reviewed");
    assert_eq!(later["status"], "conflicted");
    let conflicts = queue["manual_conflicts"].as_array().expect("conflicts");
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0]["firmware"], "2.10");
    assert_eq!(
        conflicts[0]["options"].as_array().expect("options").len(),
        2
    );
}

#[tokio::test]
async fn three_linked_owners_count_toward_agreement() {
    let mut server = TestServer::start().await;
    let owner_one = server.persist_credential().await;
    insert_serial_contribution(&server.db, "contrib-owner-one", &owner_one, [1; 32]).await;
    let camera = documented_camera("fujifilm/x-t5");
    let confirmation = confirmation_for(&camera, "native_simulations", "2.10");
    let (status, submitted) =
        post_standalone_confirmation_response(&mut server, &confirmation).await;
    assert_eq!(status, StatusCode::OK, "owner one: {submitted}");
    assert_eq!(submitted["receipt"]["owner_linkage"], "linked");

    let (owner_two, owner_two_id) = synced_owner(&server, b"owner-two", "Owner Two").await;
    insert_serial_contribution(&server.db, "contrib-owner-two", &owner_two_id, [2; 32]).await;
    let (owner_three, owner_three_id) = synced_owner(&server, b"owner-three", "Owner Three").await;
    insert_serial_contribution(&server.db, "contrib-owner-three", &owner_three_id, [3; 32]).await;
    for passkey in [&owner_two, &owner_three] {
        let request = synced_confirmation_request(&server, passkey, &confirmation).await;
        let (status, body) = post_confirmation(&server.app, &request).await;
        assert_eq!(status, StatusCode::OK, "linked owner: {body}");
        assert_eq!(body["receipt"]["owner_linkage"], "linked");
    }

    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    let control = summary_control(&summary, "native_simulations");
    assert_eq!(control["state"], "confirmed");
    assert_eq!(control["agreeing_groups"], 3);

    let token = server.moderator_token().await;
    let queue = moderation_queue(&server, &token).await;
    let checks = queue["manual_checks"].as_array().expect("manual checks");
    assert_eq!(checks.len(), 3);
    assert!(
        checks
            .iter()
            .all(|check| check["status"] == "counted" && check["owner_linkage"] == "linked")
    );
}

#[tokio::test]
async fn moderator_decision_without_watermark_counts_agreeing_check() {
    let mut server = TestServer::start().await;
    let credential = server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let confirmation = confirmation_for(&camera, "native_simulations", "2.10");
    let (status, _) = post_standalone_confirmation_response(&mut server, &confirmation).await;
    assert_eq!(status, StatusCode::OK);

    let key = CapabilityKey::NativeSimulations;
    let definition_digest = camera
        .capabilities
        .iter()
        .find(|capability| capability.key == key)
        .expect("control")
        .definition_digest(&camera);
    let scope =
        serde_json::to_string(&ModeratorFirmwareScope::exact("2.10").expect("firmware scope"))
            .expect("scope json");
    sqlx::query(
        "INSERT INTO manual_moderator_confirmations
         (camera_id, capability_key, definition_digest, firmware_scope, moderator_credential,
          confirmed, evidence_kind, evidence_url, evidence_description, created_at)
         VALUES (?, ?, ?, ?, ?, 1, 'physical', NULL, 'historical body review', 0)",
    )
    .bind(camera.id.as_str())
    .bind(key.as_str())
    .bind(definition_digest.as_bytes().as_slice())
    .bind(scope.as_str())
    .bind(credential.as_str())
    .execute(&server.db)
    .await
    .expect("historical moderator record");
    let payload = json!({
        "event": "manual_moderator_confirmation",
        "camera": camera.id.as_str(),
        "control": key.as_str(),
        "definition_digest": definition_digest.to_hex(),
        "decision": "confirm",
        "firmware_scope": {"kind": "exact", "firmware": "2.10"},
        "moderator": credential.as_str(),
        "evidence_kind": "physical",
        "evidence_url": null,
    });
    sqlx::query(
        "INSERT INTO audit_log (type, payload, ts) VALUES ('manual_moderator_confirmation', ?, 0)",
    )
    .bind(payload.to_string())
    .execute(&server.db)
    .await
    .expect("historical audit row");

    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    assert_eq!(
        summary_control(&summary, "native_simulations")["state"],
        "moderator_confirmed"
    );

    let token = server.moderator_token().await;
    let queue = moderation_queue(&server, &token).await;
    let checks = queue["manual_checks"].as_array().expect("manual checks");
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0]["status"], "counted");
    assert_eq!(checks[0]["owner_linkage"], "unlinked");
    assert!(
        queue["manual_conflicts"]
            .as_array()
            .expect("conflicts")
            .is_empty()
    );
}

#[tokio::test]
async fn moderator_scope_leaves_outside_firmware_counted() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let inside = confirmation_for(&camera, "native_simulations", "2.10");
    assert_eq!(
        post_standalone_confirmation(&mut server, &inside).await,
        StatusCode::OK
    );
    let (outside_owner, _) = synced_owner(&server, b"outside-owner", "Outside Owner").await;
    let outside = confirmation_for(&camera, "native_simulations", "3.00");
    let request = synced_confirmation_request(&server, &outside_owner, &outside).await;
    let (status, body) = post_confirmation(&server.app, &request).await;
    assert_eq!(status, StatusCode::OK, "outside firmware: {body}");

    let token = server.moderator_token().await;
    let key = CapabilityKey::NativeSimulations;
    let definition_digest = camera
        .capabilities
        .iter()
        .find(|capability| capability.key == key)
        .expect("control")
        .definition_digest(&camera);
    let moderation = ManualModeration {
        camera: camera.id.clone(),
        key,
        definition_digest,
        confirmed: true,
        evidence: ModeratorEvidence {
            kind: ModeratorEvidenceKind::Physical,
            url: None,
            description: "body review".to_owned(),
            firmware_scope: ModeratorFirmwareScope::exact("2.10").expect("scope"),
        },
    };
    let body = json!({
        "definition_digest": definition_digest,
        "confirmed": true,
        "evidence": {
            "kind": "physical",
            "description": "body review",
            "firmware_scope": {"kind": "exact", "firmware": "2.10"},
        },
    });
    let path = "/api/manual-confirmations/fujifilm/x-t5/native_simulations/moderate";
    let (status, summary) = post_signed_with_token(
        &mut server,
        path,
        &token,
        "moderate-manual-confirmation",
        &moderation.canonical_bytes(),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "moderate: {summary}");

    let queue = moderation_queue(&server, &token).await;
    let checks = queue["manual_checks"].as_array().expect("manual checks");
    assert_eq!(checks.len(), 2);
    let inside = checks
        .iter()
        .find(|check| check["firmware"] == "2.10")
        .expect("inside firmware");
    let outside = checks
        .iter()
        .find(|check| check["firmware"] == "3.00")
        .expect("outside firmware");
    assert_eq!(inside["status"], "reviewed");
    assert_eq!(outside["status"], "counted");
    assert!(
        queue["manual_conflicts"]
            .as_array()
            .expect("conflicts")
            .is_empty()
    );
}

#[tokio::test]
async fn withdrawn_photo_linkage_reads_unlinked() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let serial = RawSerial::from_envelope_ascii(SERIAL.as_bytes()).expect("serial");
    let (status, body) = server.upload_valid(Some(&serial)).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
    let contribution = body["contribution_id"]
        .as_str()
        .expect("contribution id")
        .to_owned();
    let confirmation = confirmation_for(&camera, "native_simulations", "2.10");
    let (status, submitted) =
        post_standalone_confirmation_response(&mut server, &confirmation).await;
    assert_eq!(status, StatusCode::OK, "confirmation: {submitted}");
    assert_eq!(submitted["receipt"]["owner_linkage"], "linked");

    let token = server.moderator_token().await;
    let queue = moderation_queue(&server, &token).await;
    let checks = queue["manual_checks"].as_array().expect("manual checks");
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0]["owner_linkage"], "linked");

    let mut withdraw = server
        .signed_fields("withdraw", contribution.as_bytes())
        .await;
    withdraw["contribution_ids"] = json!([contribution]);
    let (status, response) = post_json(&server.app, "/api/withdraw", &withdraw).await;
    assert_eq!(status, StatusCode::OK, "withdraw: {response}");

    let (status, resubmitted) =
        post_standalone_confirmation_response(&mut server, &confirmation).await;
    assert_eq!(status, StatusCode::OK, "resubmit: {resubmitted}");
    assert_eq!(resubmitted["receipt"]["owner_linkage"], "unlinked");
    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    let control = summary_control(&summary, "native_simulations");
    assert_eq!(control["intake"]["recorded"], 1);
    assert_eq!(control["intake"]["unlinked"], 1);

    let queue = moderation_queue(&server, &token).await;
    let checks = queue["manual_checks"].as_array().expect("manual checks");
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0]["owner_linkage"], "unlinked");
}

#[tokio::test]
async fn unspecified_moderator_scope_reviews_earlier_checks_only() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let earlier = confirmation_for(&camera, "native_simulations", "2.10");
    assert_eq!(
        post_standalone_confirmation(&mut server, &earlier).await,
        StatusCode::OK
    );

    let token = server.moderator_token().await;
    let key = CapabilityKey::NativeSimulations;
    let definition_digest = camera
        .capabilities
        .iter()
        .find(|capability| capability.key == key)
        .expect("control")
        .definition_digest(&camera);
    let moderation = ManualModeration {
        camera: camera.id.clone(),
        key,
        definition_digest,
        confirmed: true,
        evidence: ModeratorEvidence {
            kind: ModeratorEvidenceKind::Physical,
            url: None,
            description: "body review".to_owned(),
            firmware_scope: ModeratorFirmwareScope::unspecified(),
        },
    };
    let body = json!({
        "definition_digest": definition_digest,
        "confirmed": true,
        "evidence": {
            "kind": "physical",
            "description": "body review",
            "firmware_scope": {"kind": "unspecified"},
        },
    });
    let path = "/api/manual-confirmations/fujifilm/x-t5/native_simulations/moderate";
    let (status, summary) = post_signed_with_token(
        &mut server,
        path,
        &token,
        "moderate-manual-confirmation",
        &moderation.canonical_bytes(),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "moderate: {summary}");
    assert_eq!(
        summary_control(&summary, "native_simulations")["state"],
        "owner_reported"
    );

    let (later_owner, _) = synced_owner(&server, b"later-unspecified", "Later Owner").await;
    let later = confirmation_for(&camera, "native_simulations", "3.00");
    let request = synced_confirmation_request(&server, &later_owner, &later).await;
    let (status, submitted) = post_confirmation(&server.app, &request).await;
    assert_eq!(status, StatusCode::OK, "later: {submitted}");

    let queue = moderation_queue(&server, &token).await;
    let checks = queue["manual_checks"].as_array().expect("manual checks");
    assert_eq!(checks.len(), 2);
    let earlier = checks
        .iter()
        .find(|check| check["firmware"] == "2.10")
        .expect("earlier check");
    let later = checks
        .iter()
        .find(|check| check["firmware"] == "3.00")
        .expect("later check");
    assert_eq!(earlier["status"], "reviewed");
    assert_eq!(later["status"], "counted");
    assert!(
        queue["manual_conflicts"]
            .as_array()
            .expect("conflicts")
            .is_empty()
    );
}

#[tokio::test]
async fn moderator_rejection_reviews_earlier_covered_checks() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let earlier = confirmation_for(&camera, "native_simulations", "2.10");
    assert_eq!(
        post_standalone_confirmation(&mut server, &earlier).await,
        StatusCode::OK
    );

    let token = server.moderator_token().await;
    let key = CapabilityKey::NativeSimulations;
    let definition_digest = camera
        .capabilities
        .iter()
        .find(|capability| capability.key == key)
        .expect("control")
        .definition_digest(&camera);
    let moderation = ManualModeration {
        camera: camera.id.clone(),
        key,
        definition_digest,
        confirmed: false,
        evidence: ModeratorEvidence {
            kind: ModeratorEvidenceKind::Physical,
            url: None,
            description: "body review".to_owned(),
            firmware_scope: ModeratorFirmwareScope::exact("2.10").expect("scope"),
        },
    };
    let body = json!({
        "definition_digest": definition_digest,
        "confirmed": false,
        "evidence": {
            "kind": "physical",
            "description": "body review",
            "firmware_scope": {"kind": "exact", "firmware": "2.10"},
        },
    });
    let path = "/api/manual-confirmations/fujifilm/x-t5/native_simulations/moderate";
    let (status, summary) = post_signed_with_token(
        &mut server,
        path,
        &token,
        "moderate-manual-confirmation",
        &moderation.canonical_bytes(),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "moderate: {summary}");

    let (later_owner, _) = synced_owner(&server, b"later-rejected", "Later Owner").await;
    let later = confirmation_with(
        &camera,
        "native_simulations",
        "2.10",
        Finding::Corrected {
            availability: looklab_schema::Availability::Unavailable,
            domain: looklab_schema::CapabilityDomain::Unspecified,
        },
    );
    let request = synced_confirmation_request(&server, &later_owner, &later).await;
    let (status, submitted) = post_confirmation(&server.app, &request).await;
    assert_eq!(status, StatusCode::OK, "later: {submitted}");

    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    assert_eq!(
        summary_control(&summary, "native_simulations")["state"],
        "moderator_rejected"
    );
    let queue = moderation_queue(&server, &token).await;
    let checks = queue["manual_checks"].as_array().expect("manual checks");
    assert_eq!(checks.len(), 2);
    let earlier = checks
        .iter()
        .find(|check| check["finding"]["kind"] == "matches")
        .expect("earlier matching check");
    let later = checks
        .iter()
        .find(|check| check["finding"]["kind"] == "corrected")
        .expect("later corrected check");
    assert_eq!(earlier["status"], "reviewed");
    assert_eq!(later["status"], "conflicted");
    let conflicts = queue["manual_conflicts"].as_array().expect("conflicts");
    assert_eq!(conflicts.len(), 1);
    assert_eq!(
        conflicts[0]["options"].as_array().expect("options").len(),
        2
    );
}

#[tokio::test]
async fn newer_definition_answer_replaces_the_stale_check() {
    let mut server = TestServer::start().await;
    let credential = server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    insert_confirmation(
        &server.db,
        &credential,
        &camera,
        "native_simulations",
        r#"{"kind":"matches"}"#,
        21,
        0,
    )
    .await;
    sqlx::query(
        "UPDATE manual_observations SET definition_digest =
         x'0000000000000000000000000000000000000000000000000000000000000000'
         WHERE confirmation_id IN (SELECT id FROM manual_confirmations
                                   WHERE credential_id = ? AND camera_id = ?
                                     AND send_nonce IS NULL)",
    )
    .bind(credential.as_str())
    .bind(camera.id.as_str())
    .execute(&server.db)
    .await
    .expect("older definition");

    let confirmation = confirmation_for(&camera, "native_simulations", "2.10");
    let (status, submitted) =
        post_standalone_confirmation_response(&mut server, &confirmation).await;
    assert_eq!(status, StatusCode::OK, "confirmation: {submitted}");

    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    let control = summary_control(&summary, "native_simulations");
    assert_eq!(control["intake"]["stale"], 0);
    assert_eq!(control["intake"]["recorded"], 1);

    let token = server.moderator_token().await;
    let queue = moderation_queue(&server, &token).await;
    let checks = queue["manual_checks"].as_array().expect("manual checks");
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0]["status"], "counted");
}

#[tokio::test]
async fn owner_withdrawal_clears_the_check_from_queue_and_intake() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let confirmation = confirmation_for(&camera, "native_simulations", "2.10");
    let (status, submitted) =
        post_standalone_confirmation_response(&mut server, &confirmation).await;
    assert_eq!(status, StatusCode::OK, "confirmation: {submitted}");

    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    assert_eq!(
        summary_control(&summary, "native_simulations")["intake"]["recorded"],
        1
    );
    let token = server.moderator_token().await;
    let queue = moderation_queue(&server, &token).await;
    assert_eq!(
        queue["manual_checks"]
            .as_array()
            .expect("manual checks")
            .len(),
        1
    );

    let request = server.withdrawal_request(&camera.id).await;
    let (status, body) = post_withdrawal(&server.app, &request).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "withdraw: {body}");

    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    let control = summary_control(&summary, "native_simulations");
    assert_eq!(control["intake"]["recorded"], 0);
    assert_eq!(control["state"], "unconfirmed");
    assert!(
        control
            .get("firmware")
            .is_none_or(|value| { value.as_array().is_none_or(Vec::is_empty) })
    );
    let queue = moderation_queue(&server, &token).await;
    assert!(
        queue["manual_checks"]
            .as_array()
            .expect("manual checks")
            .is_empty()
    );
    assert!(
        queue["manual_conflicts"]
            .as_array()
            .expect("conflicts")
            .is_empty()
    );
}

#[tokio::test]
async fn identical_same_second_checks_keep_submission_order() {
    let mut server = TestServer::start().await;
    let owner_one = server.persist_credential().await;
    insert_serial_contribution(&server.db, "contrib-order-one", &owner_one, [5; 32]).await;
    let camera = documented_camera("fujifilm/x-t5");
    let confirmation = confirmation_for(&camera, "native_simulations", "2.10");
    let (status, submitted) =
        post_standalone_confirmation_response(&mut server, &confirmation).await;
    assert_eq!(status, StatusCode::OK, "first: {submitted}");
    assert_eq!(submitted["receipt"]["owner_linkage"], "linked");

    let (owner_two, _) = synced_owner(&server, b"order-two", "Order Two").await;
    let request = synced_confirmation_request(&server, &owner_two, &confirmation).await;
    let (status, submitted) = post_confirmation(&server.app, &request).await;
    assert_eq!(status, StatusCode::OK, "second: {submitted}");
    assert_eq!(submitted["receipt"]["owner_linkage"], "unlinked");

    sqlx::query(
        "UPDATE manual_confirmation_occurrences SET received_at = 424242
         WHERE id IN (SELECT occ.id FROM manual_confirmation_occurrences occ
                      JOIN manual_confirmations c ON c.id = occ.confirmation_id
                      WHERE c.camera_id = ?)",
    )
    .bind(camera.id.as_str())
    .execute(&server.db)
    .await
    .expect("same second");

    let token = server.moderator_token().await;
    let first = moderation_queue(&server, &token).await;
    let second = moderation_queue(&server, &token).await;
    let checks = first["manual_checks"].as_array().expect("manual checks");
    assert_eq!(checks.len(), 2);
    assert_eq!(checks[0]["owner_linkage"], "linked");
    assert_eq!(checks[1]["owner_linkage"], "unlinked");
    assert_eq!(checks[0]["finding"]["kind"], checks[1]["finding"]["kind"]);
    let repeated = second["manual_checks"].as_array().expect("manual checks");
    assert_eq!(repeated[0]["owner_linkage"], "linked");
    assert_eq!(repeated[1]["owner_linkage"], "unlinked");
}

#[tokio::test]
async fn owner_withdraws_standalone_confirmation() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let confirmation = confirmation_for(&camera, "native_simulations", "2.10");
    let digest = confirmation.digest();
    let nonce = server.nonce().await;
    let send = SendManifest::canonical_with_attestations(
        Vec::new(),
        vec![AttestationHash::from_bytes(*digest.as_bytes())],
    )
    .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = server.assertion(&challenge).await;
    let request = confirmation_request(&nonce, &send, &assertion, &confirmation);
    let (status, _) = post_confirmation(&server.app, &request).await;
    assert_eq!(status, StatusCode::OK);

    let credential = server.credential_uuid().await;
    insert_serial_contribution(&server.db, "contrib-owner", &credential, [7; 32]).await;
    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    assert_eq!(
        summary_control(&summary, "native_simulations")["agreeing_groups"],
        1
    );

    let request = server.withdrawal_request(&camera.id).await;
    let (status, _) = post_withdrawal(&server.app, &request).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let audit: String = sqlx::query_scalar(
        "SELECT payload FROM audit_log WHERE type = 'manual_confirmation_withdrawal'
         ORDER BY seq DESC LIMIT 1",
    )
    .fetch_one(&server.db)
    .await
    .expect("withdrawal audit");
    let audit: Value = serde_json::from_str(&audit).expect("audit json");
    assert_eq!(audit["camera"], camera.id.as_str());
    assert_eq!(audit["withdrawn"], 1);

    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    assert_eq!(
        summary_control(&summary, "native_simulations")["agreeing_groups"],
        0
    );

    let nonce = server.nonce().await;
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = server.assertion(&challenge).await;
    let request = confirmation_request(&nonce, &send, &assertion, &confirmation);
    let (status, _) = post_confirmation(&server.app, &request).await;
    assert_eq!(status, StatusCode::OK);
    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    assert_eq!(
        summary_control(&summary, "native_simulations")["agreeing_groups"],
        1
    );
}

#[tokio::test]
async fn owner_withdrawal_fences_a_mixed_send_without_dropping_photos() {
    let mut server = TestServer::start().await;
    let camera = documented_camera("fujifilm/x-t5");
    let confirmation = confirmation_for(&camera, "native_simulations", "2.10");
    let digest = confirmation.digest();
    let (manifest, blobs) = manifest(valid_settings());
    let nonce = server.nonce().await;
    let send = SendManifest::canonical_with_attestations(
        vec![manifest_hash(&manifest)],
        vec![AttestationHash::from_bytes(*digest.as_bytes())],
    )
    .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = server.assertion(&challenge).await;
    let serial = RawSerial::from_envelope_ascii(SERIAL.as_bytes()).expect("serial");
    let (status, body) = server
        .upload_bytes(envelope_with_send(
            nonce,
            &send,
            &manifest,
            &blobs,
            Some(&serial),
            &assertion,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
    let contribution = body["contribution_id"]
        .as_str()
        .expect("contribution id")
        .to_owned();

    let withdraw = server.withdrawal_request(&camera.id).await;
    let (status, body) = post_withdrawal(&server.app, &withdraw).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "withdraw: {body}");

    let delayed = confirmation_request(&nonce, &send, &assertion, &confirmation);
    let (status, body) = post_confirmation(&server.app, &delayed).await;
    assert_eq!(status, StatusCode::CONFLICT, "delayed: {body}");

    let contribution_status: String =
        sqlx::query_scalar("SELECT status FROM contributions WHERE id = ?")
            .bind(&contribution)
            .fetch_one(&server.db)
            .await
            .expect("contribution");
    assert_ne!(contribution_status, "withdrawn");
    let send_withdrawn: Option<i64> =
        sqlx::query_scalar("SELECT withdrawn_at FROM send_manifests WHERE nonce = ?")
            .bind(hex(nonce.as_bytes()))
            .fetch_one(&server.db)
            .await
            .expect("send");
    assert_eq!(send_withdrawn, None);

    let nonce = server.nonce().await;
    let send = SendManifest::canonical_with_attestations(
        Vec::new(),
        vec![AttestationHash::from_bytes(*digest.as_bytes())],
    )
    .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = server.assertion(&challenge).await;
    let fresh = confirmation_request(&nonce, &send, &assertion, &confirmation);
    let (status, summary) = post_confirmation(&server.app, &fresh).await;
    assert_eq!(status, StatusCode::OK, "fresh: {summary}");
}

#[tokio::test]
async fn manual_withdrawal_rejects_noncanonical_bytes_and_undocumented_cameras() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let confirmation = confirmation_for(&camera, "native_simulations", "2.10");
    assert_eq!(
        post_standalone_confirmation(&mut server, &confirmation).await,
        StatusCode::OK
    );
    let credential = server.credential_uuid().await;
    insert_serial_contribution(&server.db, "contrib-owner", &credential, [7; 32]).await;
    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    assert_eq!(
        summary_control(&summary, "native_simulations")["agreeing_groups"],
        1
    );

    let request = server.withdrawal_request(&camera.id).await;
    let mut noncanonical = request.canonical_bytes();
    noncanonical.insert(1, b' ');
    let (status, _) = post_canonical(
        &server.app,
        "/api/manual-confirmations/withdraw",
        noncanonical,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let unknown_camera = BodyId::new("unknown/camera").expect("body id");
    let unknown = server.withdrawal_request(&unknown_camera).await;
    let (status, _) = post_withdrawal(&server.app, &unknown).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    assert_eq!(
        summary_control(&summary, "native_simulations")["agreeing_groups"],
        1
    );
    let withdrawals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE type = 'manual_confirmation_withdrawal'",
    )
    .fetch_one(&server.db)
    .await
    .expect("withdrawal audits");
    assert_eq!(withdrawals, 0);
}

#[tokio::test]
async fn manual_confirmation_rejects_invalid_structure_before_persistence() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let valid = confirmation_for(&camera, "native_simulations", "2.10");

    let mut version = valid.clone();
    version.version += 1;
    let mut empty = valid.clone();
    empty.observations.clear();
    let mut duplicate = valid.clone();
    let first = duplicate.observations[0].clone();
    duplicate.observations.push(first);
    let mut mismatched = valid;
    mismatched.scope = ConfirmationScope::Controls {
        keys: vec![CapabilityKey::ExposureCompensation],
    };

    for confirmation in [version, empty, duplicate, mismatched] {
        assert_eq!(
            post_standalone_confirmation(&mut server, &confirmation).await,
            StatusCode::BAD_REQUEST
        );
    }

    let confirmation = confirmation_for(&camera, "native_simulations", "2.10");
    let nonce = server.nonce().await;
    let digest = confirmation.digest();
    let send = SendManifest::canonical_with_attestations(
        Vec::new(),
        vec![AttestationHash::from_bytes(*digest.as_bytes())],
    )
    .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = server.assertion(&challenge).await;
    let request = confirmation_request(&nonce, &send, &assertion, &confirmation);
    let mut noncanonical = request.canonical_bytes();
    noncanonical.insert(1, b' ');
    let (status, _) = post_canonical(&server.app, "/api/manual-confirmations", noncanonical).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM manual_confirmations")
        .fetch_one(&server.db)
        .await
        .expect("manual confirmations");
    assert_eq!(stored, 0);
    let sends: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM send_manifests")
        .fetch_one(&server.db)
        .await
        .expect("send manifests");
    assert_eq!(sends, 0);
}

#[tokio::test]
async fn confirmation_before_photos_reserves_the_declared_batches() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let confirmation = confirmation_for(&camera, "native_simulations", "2.10");
    let digest = confirmation.digest();
    let (manifest, blobs) = manifest(valid_settings());
    let nonce = server.nonce().await;
    let send = SendManifest::canonical_with_attestations(
        vec![manifest_hash(&manifest)],
        vec![AttestationHash::from_bytes(*digest.as_bytes())],
    )
    .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = server.assertion(&challenge).await;

    let request = confirmation_request(&nonce, &send, &assertion, &confirmation);
    let (status, summary) = post_confirmation(&server.app, &request).await;
    assert_eq!(status, StatusCode::OK, "confirmation: {summary}");

    let serial = RawSerial::from_envelope_ascii(SERIAL.as_bytes()).expect("serial");
    let (status, body) = server
        .upload_bytes(envelope_with_send(
            nonce,
            &send,
            &manifest,
            &blobs,
            Some(&serial),
            &assertion,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");
}

#[tokio::test]
async fn manual_confirmation_rejects_unbound_manifests_and_content() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let confirmation = confirmation_for(&camera, "native_simulations", "2.10");
    let digest = confirmation.digest();

    let nonce = server.nonce().await;
    let batch_only = SendManifest::canonical(vec![ManifestHash::from_bytes(*digest.as_bytes())])
        .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&batch_only));
    let assertion = server.assertion(&challenge).await;
    let (status, _) = post_confirmation(
        &server.app,
        &confirmation_request(&nonce, &batch_only, &assertion, &confirmation),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let nonce = server.nonce().await;
    let same = ManifestHash::from_bytes([9u8; 32]);
    let overlapping = SendManifest {
        version: SEND_MANIFEST_VERSION,
        batches: vec![same],
        attestations: vec![AttestationHash::from_bytes(*same.as_bytes())],
    };
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&overlapping));
    let assertion = server.assertion(&challenge).await;
    let (status, _) = post_confirmation(
        &server.app,
        &confirmation_request(&nonce, &overlapping, &assertion, &confirmation),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let nonce = server.nonce().await;
    let send = SendManifest::canonical_with_attestations(
        Vec::new(),
        vec![AttestationHash::from_bytes(*digest.as_bytes())],
    )
    .expect("send manifest");
    let challenge = Challenge::for_send(&nonce, &send_manifest_hash(&send));
    let assertion = server.assertion(&challenge).await;

    let mut altered = serde_json::to_value(confirmation_request(
        &nonce,
        &send,
        &assertion,
        &confirmation,
    ))
    .expect("confirmation json");
    altered["confirmation"]["firmware"] = json!("9.99");
    let (status, _) = post_json(&server.app, "/api/manual-confirmations", &altered).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let mut extra = serde_json::to_value(confirmation_request(
        &nonce,
        &send,
        &assertion,
        &confirmation,
    ))
    .expect("confirmation json");
    extra["unbound"] = json!(true);
    let (status, _) = post_json(&server.app, "/api/manual-confirmations", &extra).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn manual_confirmation_policy_counts_identity_groups() {
    let server = TestServer::start().await;
    let camera = documented_camera("fujifilm/x-t5");
    for index in 0..3u8 {
        let credential = server.add_credential().await;
        insert_serial_contribution(
            &server.db,
            &format!("contrib-{index}"),
            &credential,
            [index + 1; 32],
        )
        .await;
        insert_confirmation(
            &server.db,
            &credential,
            &camera,
            "native_simulations",
            r#"{"kind":"matches"}"#,
            index,
            i64::from(index),
        )
        .await;
    }
    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    let control = summary_control(&summary, "native_simulations");
    assert_eq!(control["state"], "confirmed");
    assert_eq!(control["agreeing_groups"], 3);

    let shared = server.add_credential().await;
    insert_serial_contribution(&server.db, "contrib-shared", &shared, [1; 32]).await;
    insert_confirmation(
        &server.db,
        &shared,
        &camera,
        "native_simulations",
        r#"{"kind":"matches"}"#,
        10,
        10,
    )
    .await;
    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    assert_eq!(
        summary_control(&summary, "native_simulations")["agreeing_groups"],
        3
    );

    let contradicting = server.add_credential().await;
    insert_serial_contribution(&server.db, "contrib-contra", &contradicting, [4; 32]).await;
    insert_confirmation(
        &server.db,
        &contradicting,
        &camera,
        "native_simulations",
        r#"{"kind":"corrected","availability":"unavailable","domain":{"type":"unspecified"}}"#,
        11,
        11,
    )
    .await;
    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    let control = summary_control(&summary, "native_simulations");
    assert_eq!(control["state"], "contradicted");
    assert_eq!(control["agreeing_groups"], 3);
    assert!(
        !control["contradictions"]
            .as_array()
            .expect("contradictions")
            .is_empty()
    );

    insert_confirmation(
        &server.db,
        &contradicting,
        &camera,
        "native_simulations",
        r#"{"kind":"matches"}"#,
        12,
        12,
    )
    .await;
    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    assert_eq!(
        summary_control(&summary, "native_simulations")["state"],
        "confirmed"
    );
}

#[tokio::test]
async fn repeated_payload_after_a_correction_uses_the_new_occurrence() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let credential = server.credential_uuid().await;
    insert_serial_contribution(&server.db, "contrib-occ", &credential, [9; 32]).await;
    for index in 0..2u8 {
        let other = server.add_credential().await;
        insert_serial_contribution(
            &server.db,
            &format!("contrib-occ-{index}"),
            &other,
            [index + 1; 32],
        )
        .await;
        insert_confirmation(
            &server.db,
            &other,
            &camera,
            "native_simulations",
            r#"{"kind":"matches"}"#,
            index,
            i64::from(index),
        )
        .await;
    }
    let matches = confirmation_for(&camera, "native_simulations", "2.10");
    let corrected = confirmation_with(
        &camera,
        "native_simulations",
        "2.10",
        Finding::Corrected {
            availability: looklab_schema::Availability::Unavailable,
            domain: looklab_schema::CapabilityDomain::Unspecified,
        },
    );

    assert_eq!(
        post_standalone_confirmation(&mut server, &matches).await,
        StatusCode::OK
    );
    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    assert_eq!(
        summary_control(&summary, "native_simulations")["state"],
        "confirmed"
    );

    assert_eq!(
        post_standalone_confirmation(&mut server, &corrected).await,
        StatusCode::OK
    );
    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    assert_eq!(
        summary_control(&summary, "native_simulations")["state"],
        "contradicted"
    );

    assert_eq!(
        post_standalone_confirmation(&mut server, &matches).await,
        StatusCode::OK
    );
    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    assert_eq!(
        summary_control(&summary, "native_simulations")["state"],
        "confirmed"
    );
}

#[tokio::test]
async fn same_second_linked_claims_order_by_occurrence_and_tombstone() {
    let server = TestServer::start().await;
    let camera = documented_camera("fujifilm/x-t5");
    for index in 0..2u8 {
        let other = server.add_credential().await;
        insert_serial_contribution(
            &server.db,
            &format!("contrib-same-{index}"),
            &other,
            [index + 1; 32],
        )
        .await;
        insert_confirmation(
            &server.db,
            &other,
            &camera,
            "native_simulations",
            r#"{"kind":"matches"}"#,
            index,
            i64::from(index),
        )
        .await;
    }
    let first = server.add_credential().await;
    let second = server.add_credential().await;
    insert_serial_contribution(&server.db, "contrib-link-a", &first, [7; 32]).await;
    insert_serial_contribution(&server.db, "contrib-link-b", &second, [7; 32]).await;
    insert_confirmation(
        &server.db,
        &first,
        &camera,
        "native_simulations",
        r#"{"kind":"matches"}"#,
        21,
        5,
    )
    .await;
    insert_confirmation(
        &server.db,
        &second,
        &camera,
        "native_simulations",
        r#"{"kind":"corrected","availability":"unavailable","domain":{"type":"unspecified"}}"#,
        22,
        5,
    )
    .await;

    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    assert_eq!(
        summary_control(&summary, "native_simulations")["state"],
        "contradicted"
    );

    sqlx::query(
        "UPDATE manual_confirmation_occurrences SET withdrawn_at = 1 WHERE send_nonce = 'seed-16'",
    )
    .execute(&server.db)
    .await
    .expect("withdraw occurrence");

    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    let control = summary_control(&summary, "native_simulations");
    assert_eq!(control["state"], "owner_reported");
    assert_eq!(control["agreeing_groups"], 2);
    assert_eq!(control["firmware"][0]["reported_profiles"], 2);
}

#[tokio::test]
async fn latest_claims_keep_each_reported_firmware_version() {
    let server = TestServer::start().await;
    let camera = documented_camera("fujifilm/x-t5");
    for index in 0..3u8 {
        let credential = server.add_credential().await;
        insert_serial_contribution(
            &server.db,
            &format!("contrib-fw-{index}"),
            &credential,
            [5 + index; 32],
        )
        .await;
        for (offset, version) in ["2.10", "3.00"].into_iter().enumerate() {
            insert_confirmation_with_firmware(
                &server.db,
                &credential,
                &camera,
                "native_simulations",
                &ReportedFirmware::new(version).expect("firmware"),
                r#"{"kind":"matches"}"#,
                ConfirmationOccurrence {
                    payload: 30 + 2 * index + offset as u8,
                    created_at: offset as i64,
                },
            )
            .await;
        }
    }

    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    let control = summary_control(&summary, "native_simulations");
    let firmware = control["firmware"].as_array().expect("firmware");
    assert_eq!(firmware.len(), 2);
    assert_eq!(firmware[0]["firmware"], "2.10");
    assert_eq!(firmware[1]["firmware"], "3.00");
    assert_eq!(firmware[0]["agreeing_groups"], 3);
    assert_eq!(firmware[1]["agreeing_groups"], 3);
    assert_eq!(control["state"], "confirmed");
}

#[tokio::test]
async fn fresh_credentials_without_serial_evidence_are_owner_reported_not_confirmed() {
    let server = TestServer::start().await;
    let camera = documented_camera("fujifilm/x-t5");
    for index in 0..3u8 {
        let credential = server.add_credential().await;
        insert_confirmation(
            &server.db,
            &credential,
            &camera,
            "native_simulations",
            r#"{"kind":"matches"}"#,
            index,
            i64::from(index),
        )
        .await;
    }
    let summary = manual_summary(&server, "Fujifilm", "X-T5").await;
    let control = summary_control(&summary, "native_simulations");
    assert_eq!(control["state"], "owner_reported");
    assert_eq!(control["agreeing_groups"], 0);
    assert_eq!(control["firmware"][0]["owner_profiles"], 3);
}

#[tokio::test]
async fn moderator_evidence_is_role_gated_and_audited() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    server
        .set_credential_tier_roles(&server.credential_id, "new", "[\"moderator\"]")
        .await;
    let signed = server.signed_fields("mod-session", &[]).await;
    let (status, session) = post_json(&server.app, "/api/mod/session", &signed).await;
    assert_eq!(status, StatusCode::OK, "session: {session}");
    let token = session["session_token"]
        .as_str()
        .expect("session token")
        .to_owned();
    let path = "/api/manual-confirmations/fujifilm/x-t5/native_simulations/moderate";
    let camera = documented_camera("fujifilm/x-t5");
    let key = CapabilityKey::NativeSimulations;
    let definition_digest = camera
        .capabilities
        .iter()
        .find(|capability| capability.key == key)
        .expect("control")
        .definition_digest(&camera);
    let firmware = ModeratorFirmwareScope::exact("2.10").expect("firmware scope");

    let missing_url_evidence = ModeratorEvidence {
        kind: ModeratorEvidenceKind::Video,
        url: None,
        description: "menu walkthrough".to_owned(),
        firmware_scope: firmware.clone(),
    };
    let missing_url = ManualModeration {
        camera: camera.id.clone(),
        key,
        definition_digest,
        confirmed: true,
        evidence: missing_url_evidence,
    };
    let body = json!({
        "definition_digest": definition_digest,
        "confirmed": true,
        "evidence": {
            "kind": "video",
            "description": "menu walkthrough",
            "firmware_scope": {"kind": "exact", "firmware": "2.10"},
        },
    });
    let (status, _) = post_signed_with_token(
        &mut server,
        path,
        &token,
        "moderate-manual-confirmation",
        &missing_url.canonical_bytes(),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let decision_evidence = ModeratorEvidence {
        kind: ModeratorEvidenceKind::Video,
        url: Some(ManualUrl::new("https://example.com/review").expect("evidence url")),
        description: "menu walkthrough".to_owned(),
        firmware_scope: firmware,
    };
    let decision = ManualModeration {
        camera: camera.id.clone(),
        key,
        definition_digest,
        confirmed: true,
        evidence: decision_evidence,
    };
    let body = json!({
        "definition_digest": definition_digest,
        "confirmed": true,
        "evidence": {
            "kind": "video",
            "url": "https://example.com/review",
            "description": "menu walkthrough",
            "firmware_scope": {"kind": "exact", "firmware": "2.10"},
        },
    });
    let (status, summary) = post_signed_with_token(
        &mut server,
        path,
        &token,
        "moderate-manual-confirmation",
        &decision.canonical_bytes(),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "moderate: {summary}");
    assert_eq!(
        summary_control(&summary, "native_simulations")["state"],
        "moderator_confirmed"
    );
    let audit: String = sqlx::query_scalar(
        "SELECT payload FROM audit_log WHERE type = 'manual_moderator_confirmation'
         ORDER BY seq DESC LIMIT 1",
    )
    .fetch_one(&server.db)
    .await
    .expect("moderator audit");
    let audit: Value = serde_json::from_str(&audit).expect("audit json");
    assert_eq!(audit["firmware_scope"]["kind"], "exact");
    assert_eq!(audit["firmware_scope"]["firmware"], "2.10");

    let standing: (String, String) =
        sqlx::query_as("SELECT tier, roles FROM credentials WHERE credential_id = ?")
            .bind(&server.credential_id)
            .fetch_one(&server.db)
            .await
            .expect("standing");
    assert_eq!(standing, ("new".to_owned(), "[\"moderator\"]".to_owned()));

    server
        .set_credential_tier_roles(&server.credential_id, "new", "[]")
        .await;
    let body = json!({
        "definition_digest": definition_digest,
        "confirmed": true,
        "evidence": {
            "kind": "video",
            "url": "https://example.com/review",
            "description": "menu walkthrough",
            "firmware_scope": {"kind": "exact", "firmware": "2.10"},
        },
    });
    let (status, _) = post_signed_with_token(
        &mut server,
        path,
        &token,
        "moderate-manual-confirmation",
        &decision.canonical_bytes(),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn moderator_decision_rejects_a_changed_control_definition() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    server
        .set_credential_tier_roles(&server.credential_id, "new", "[\"moderator\"]")
        .await;
    let signed = server.signed_fields("mod-session", &[]).await;
    let (status, session) = post_json(&server.app, "/api/mod/session", &signed).await;
    assert_eq!(status, StatusCode::OK, "session: {session}");
    let token = session["session_token"]
        .as_str()
        .expect("session token")
        .to_owned();
    let camera = documented_camera("fujifilm/x-t5");
    let key = CapabilityKey::NativeSimulations;
    let stale = Sha256Hex::from_bytes([0; 32]);
    let moderation = ManualModeration {
        camera: camera.id.clone(),
        key,
        definition_digest: stale,
        confirmed: true,
        evidence: ModeratorEvidence {
            kind: ModeratorEvidenceKind::Physical,
            url: None,
            description: "body review".to_owned(),
            firmware_scope: ModeratorFirmwareScope::unspecified(),
        },
    };
    let body = json!({
        "definition_digest": stale,
        "confirmed": true,
        "evidence": {
            "kind": "physical",
            "description": "body review",
            "firmware_scope": {"kind": "unspecified"},
        },
    });
    let path = "/api/manual-confirmations/fujifilm/x-t5/native_simulations/moderate";
    let (status, _) = post_signed_with_token(
        &mut server,
        path,
        &token,
        "moderate-manual-confirmation",
        &moderation.canonical_bytes(),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM manual_moderator_confirmations")
        .fetch_one(&server.db)
        .await
        .expect("moderator rows");
    assert_eq!(stored, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_old_envelope_version_requests_a_reload() {
    let mut server = TestServer::start().await;
    let (manifest, blobs) = manifest(valid_settings());
    let nonce = server.nonce().await;
    let challenge = send_challenge(&nonce, &manifest);
    let assertion = server.assertion(&challenge).await;
    let pow = solve(
        &Challenge::new(&nonce, &manifest_hash(&manifest)),
        u32::try_from(manifest.images.len()).expect("image count"),
        &pow_params(),
        0,
    )
    .expect("proof-of-work");
    let mut bytes = envelope(nonce, &manifest, &blobs, None, &assertion, &pow);
    bytes[6..8].copy_from_slice(&3u16.to_le_bytes());
    let (status, body) = server.upload_bytes(bytes).await;
    assert_eq!(status, StatusCode::UPGRADE_REQUIRED, "upload: {body}");
    assert_eq!(body["error"], "ENVELOPE_VERSION_UNSUPPORTED");
    assert_eq!(body["action"], "reload_and_prepare");
}

#[tokio::test(flavor = "multi_thread")]
async fn photo_arrival_audits_a_manual_promotion() {
    let mut server = TestServer::start().await;
    let owner = server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    for index in 0..2usize {
        let credential = server.add_credential().await;
        insert_serial_contribution(
            &server.db,
            &format!("promotion-{index}"),
            &credential,
            [u8::try_from(index + 1).expect("small index"); 32],
        )
        .await;
        insert_confirmation(
            &server.db,
            &credential,
            &camera,
            "native_simulations",
            r#"{"kind":"matches"}"#,
            u8::try_from(index + 1).expect("small index"),
            i64::try_from(index).expect("small index"),
        )
        .await;
    }
    insert_confirmation(
        &server.db,
        &owner,
        &camera,
        "native_simulations",
        r#"{"kind":"matches"}"#,
        9,
        10,
    )
    .await;
    let before = manual_summary(&server, "Fujifilm", "X-T5").await;
    assert_eq!(
        summary_control(&before, "native_simulations")["agreeing_groups"],
        2
    );

    let serial = RawSerial::new(SERIAL.as_bytes()).expect("serial");
    let (status, body) = server.upload_valid(Some(&serial)).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");

    let after = manual_summary(&server, "Fujifilm", "X-T5").await;
    let control = summary_control(&after, "native_simulations");
    assert_eq!(control["state"], "confirmed");
    assert_eq!(control["agreeing_groups"], 3);
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE type = 'manual_confirmation_promotion'",
    )
    .fetch_one(&server.db)
    .await
    .expect("promotion audits");
    assert_eq!(audits, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn withdrawal_reconciles_a_new_promotion() {
    let mut server = TestServer::start().await;
    let owner = server.persist_credential().await;
    let camera = documented_camera("fujifilm/x-t5");
    let contribution = uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0001).to_string();
    insert_serial_contribution(&server.db, contribution.as_str(), &owner, [1; 32]).await;
    insert_confirmation(
        &server.db,
        &owner,
        &camera,
        "native_simulations",
        r#"{"kind":"corrected","availability":"unavailable","domain":{"type":"unspecified"}}"#,
        1,
        1,
    )
    .await;
    for index in 0..3usize {
        let credential = server.add_credential().await;
        let matching =
            uuid::Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0002 + index as u128)
                .to_string();
        insert_serial_contribution(
            &server.db,
            matching.as_str(),
            &credential,
            [u8::try_from(index + 2).expect("small index"); 32],
        )
        .await;
        insert_confirmation(
            &server.db,
            &credential,
            &camera,
            "native_simulations",
            r#"{"kind":"matches"}"#,
            u8::try_from(index + 2).expect("small index"),
            i64::try_from(index + 2).expect("small index"),
        )
        .await;
    }
    let before = manual_summary(&server, "Fujifilm", "X-T5").await;
    assert_eq!(
        summary_control(&before, "native_simulations")["state"],
        "contradicted"
    );

    let mut withdraw = server
        .signed_fields("withdraw", contribution.as_bytes())
        .await;
    withdraw["contribution_ids"] = json!([contribution.as_str()]);
    let (status, response) = post_json(&server.app, "/api/withdraw", &withdraw).await;
    assert_eq!(status, StatusCode::OK, "withdraw: {response}");

    let after = manual_summary(&server, "Fujifilm", "X-T5").await;
    assert_eq!(
        summary_control(&after, "native_simulations")["state"],
        "confirmed"
    );
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE type = 'manual_confirmation_promotion'",
    )
    .fetch_one(&server.db)
    .await
    .expect("promotion audits");
    assert_eq!(audits, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_requests_metadata_publication_without_looks() {
    let server = TestServer::start().await;
    let generation: i64 = sqlx::query_scalar("SELECT generation FROM publication WHERE id = 1")
        .fetch_one(&server.db)
        .await
        .expect("publication row");
    assert!(generation >= 1);
}

fn client_error_report(release: &str) -> Value {
    json!({
        "version": 1,
        "kind": "console_error",
        "error_type": "TypeError",
        "page": "home",
        "release": release,
        "frames": [{"asset": "pkg/looklab-site.js", "line": 123, "column": 45}],
    })
}

async fn post_client_error(app: &Router, body: Vec<u8>) -> StatusCode {
    let request = Request::builder()
        .method("POST")
        .uri("/api/client-errors")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("request");
    send(app, request).await.0
}

#[tokio::test]
async fn client_error_reports_are_accepted_without_a_public_read() {
    let server = TestServer::start().await;
    let release = "a".repeat(64);
    let status = post_client_error(
        &server.app,
        serde_json::to_vec(&client_error_report(&release)).expect("json"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let rows: Vec<(String, String, String, String, String, i64)> = sqlx::query_as(
        "SELECT release, kind, error_type, page, frames, received_at FROM client_errors",
    )
    .fetch_all(&server.db)
    .await
    .expect("client error rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, release);
    assert_eq!(rows[0].1, "console_error");
    assert_eq!(rows[0].2, "TypeError");
    assert_eq!(rows[0].3, "home");
    assert_eq!(
        rows[0].4,
        r#"[{"asset":"pkg/looklab-site.js","line":123,"column":45}]"#
    );
    assert!(rows[0].5 > 0);
    let (status, _, _) = server.get("/api/client-errors").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn client_error_reports_require_json_media_type() {
    let server = TestServer::start().await;
    let release = "9".repeat(64);
    let body = serde_json::to_vec(&client_error_report(&release)).expect("json");
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
        Some("application/jsonx"),
        Some("json"),
        Some("application/json, text/plain"),
    ] {
        let mut builder = Request::builder().method("POST").uri("/api/client-errors");
        if let Some(content_type) = content_type {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        let request = builder.body(Body::from(body.clone())).expect("request");
        let (status, _, _) = send(&server.app, request).await;
        assert_eq!(
            status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content type {content_type:?}"
        );
    }
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM client_errors")
        .fetch_one(&server.db)
        .await
        .expect("client error rows");
    assert_eq!(stored, 0);
    for content_type in [
        "application/json",
        "application/json; charset=utf-8",
        "Application/JSON;charset=UTF-8",
        "application/json ; charset=\"utf-8\"",
    ] {
        let request = Request::builder()
            .method("POST")
            .uri("/api/client-errors")
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(body.clone()))
            .expect("request");
        let (status, _, _) = send(&server.app, request).await;
        assert_eq!(
            status,
            StatusCode::NO_CONTENT,
            "content type {content_type}"
        );
    }
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM client_errors")
        .fetch_one(&server.db)
        .await
        .expect("client error rows");
    assert_eq!(stored, 4);
}

#[tokio::test]
async fn client_error_reports_accept_every_allowed_asset() {
    let server = TestServer::start().await;
    let release = "b".repeat(64);
    for asset in [
        "errors.js",
        "pkg/looklab-site.js",
        "pkg/looklab-site-0123abcd.wasm",
        "workers/prep-worker.js",
        "workers/match-worker.js",
        "workers/scene-worker.js",
        "generated/wasm/looklab_prep_module.wasm",
        "generated/wasm/looklab_scene_module.wasm",
        "generated/wasm/looklab_match.wasm",
    ] {
        let mut value = client_error_report(&release);
        value["frames"] = json!([{"asset": asset, "line": 0, "column": 0}]);
        let status =
            post_client_error(&server.app, serde_json::to_vec(&value).expect("json")).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "asset {asset}");
    }
    let mut twelve = client_error_report(&release);
    twelve["frames"] = Value::Array(
        (0..12)
            .map(|_| json!({"asset": "pkg/looklab-site.js", "line": 1, "column": 2}))
            .collect(),
    );
    assert_eq!(
        post_client_error(&server.app, serde_json::to_vec(&twelve).expect("json")).await,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn client_error_reports_reject_invalid_fields_and_assets() {
    let server = TestServer::start().await;
    let release = "c".repeat(64);
    let mut invalid = Vec::new();
    let mut unknown_field = client_error_report(&release);
    unknown_field["message"] = json!("private text");
    invalid.push(unknown_field);
    let mut unknown_frame_field = client_error_report(&release);
    unknown_frame_field["frames"][0]["message"] = json!("private text");
    invalid.push(unknown_frame_field);
    let mut version = client_error_report(&release);
    version["version"] = json!(2);
    invalid.push(version);
    let mut kind = client_error_report(&release);
    kind["kind"] = json!("panic");
    invalid.push(kind);
    let mut error_type = client_error_report(&release);
    error_type["error_type"] = json!("UriError");
    invalid.push(error_type);
    let mut page = client_error_report(&release);
    page["page"] = json!("admin");
    invalid.push(page);
    let mut uppercase_release = client_error_report(&release);
    uppercase_release["release"] = json!("A".repeat(64));
    invalid.push(uppercase_release);
    let mut short_release = client_error_report(&release);
    short_release["release"] = json!("a".repeat(63));
    invalid.push(short_release);
    let mut too_many_frames = client_error_report(&release);
    too_many_frames["frames"] = Value::Array(
        (0..13)
            .map(|_| json!({"asset": "pkg/looklab-site.js", "line": 1, "column": 2}))
            .collect(),
    );
    invalid.push(too_many_frames);
    for asset in [
        "pkg/../errors.js",
        "pkg/sub/looklab-site.js",
        "pkg/looklab-site.css",
        "pkg/",
        "workers/other-worker.js",
        "https://looklab.example/pkg/looklab-site.js",
        "pkg/looklab-site.js?cache=1",
    ] {
        let mut value = client_error_report(&release);
        value["frames"] = json!([{"asset": asset, "line": 1, "column": 2}]);
        invalid.push(value);
    }
    let mut negative_line = client_error_report(&release);
    negative_line["frames"][0]["line"] = json!(-1);
    invalid.push(negative_line);
    let mut overflowing_line = client_error_report(&release);
    overflowing_line["frames"][0]["line"] = json!(4_294_967_296u64);
    invalid.push(overflowing_line);
    for value in invalid {
        let status =
            post_client_error(&server.app, serde_json::to_vec(&value).expect("json")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "value {value}");
    }
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM client_errors")
        .fetch_one(&server.db)
        .await
        .expect("client error rows");
    assert_eq!(stored, 0);
    assert!(!contains(&log_bytes(), b"private text"));
}

#[tokio::test]
async fn oversized_client_error_reports_are_rejected() {
    let server = TestServer::start().await;
    let mut body = serde_json::to_vec(&client_error_report(&"d".repeat(64))).expect("json");
    body.resize(16 * 1024 + 1, b' ');
    let status = post_client_error(&server.app, body).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM client_errors")
        .fetch_one(&server.db)
        .await
        .expect("client error rows");
    assert_eq!(stored, 0);
}

#[tokio::test]
async fn client_error_reports_share_one_global_quota() {
    let server = TestServer::start().await;
    let release = "e".repeat(64);
    let body = serde_json::to_vec(&client_error_report(&release)).expect("json");
    let mut accepted = 0i64;
    let mut rejected = false;
    for _ in 0..70 {
        match post_client_error(&server.app, body.clone()).await {
            StatusCode::NO_CONTENT => accepted += 1,
            StatusCode::TOO_MANY_REQUESTS => {
                rejected = true;
                break;
            }
            other => panic!("unexpected status {other}"),
        }
    }
    assert!(rejected, "the global quota never rejected a report");
    assert!(accepted >= 60, "only {accepted} reports were admitted");
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM client_errors")
        .fetch_one(&server.db)
        .await
        .expect("client error rows");
    assert_eq!(stored, accepted);
}

#[tokio::test]
async fn client_error_reports_never_reach_the_logs() {
    let server = TestServer::start().await;
    let release = "f".repeat(64);
    let mut value = client_error_report(&release);
    value["frames"] =
        json!([{"asset": "generated/wasm/looklab_match.wasm", "line": 7, "column": 9}]);
    assert_eq!(
        post_client_error(&server.app, serde_json::to_vec(&value).expect("json")).await,
        StatusCode::NO_CONTENT
    );
    let logs = log_bytes();
    assert!(!contains(&logs, release.as_bytes()));
    assert!(!contains(&logs, b"generated/wasm/looklab_match.wasm"));
    assert!(!contains(&logs, b"console_error"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_lut_upload_is_stored_and_approval_queues_one_import() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let cube = "\
TITLE \"Test\"
LUT_3D_SIZE 2
DOMAIN_MIN 0.0 0.0 0.0
DOMAIN_MAX 1.0 1.0 1.0
0.0 0.0 0.0
1.0 0.0 0.0
0.0 1.0 0.0
1.0 1.0 0.0
0.0 0.0 1.0
1.0 0.0 1.0
0.0 1.0 1.0
1.0 1.0 1.0
";
    let bytes = cube.as_bytes();
    let digest = hex(&Sha256::digest(bytes));
    let payload = format!(
        "look: test-lut\nname: Test LUT\nlicense: {{ own_work: true }}\n\
         attribution: A contributor\nlut:\n  file: test-lut.cube\n  sha256: {digest}\n  \
         size: {}\nencoding: flog2_fgamut\n",
        bytes.len()
    );
    let (status, body) = server.submit_lut("lut_look", &payload, bytes).await;
    assert_eq!(status, StatusCode::CREATED, "submit: {body}");
    let id = body["view"]["id"].as_str().expect("proposal id").to_owned();
    let uploads: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lut_uploads u
         JOIN credentials c ON c.id = u.credential_id
         WHERE c.credential_id = ?",
    )
    .bind(server.credential_id.as_slice())
    .fetch_one(&server.db)
    .await
    .expect("uploads");
    assert_eq!(uploads, 1);

    let token = server.moderator_token().await;
    let (status, decision) = server.decide(&token, &id, "import it").await;
    assert_eq!(status, StatusCode::OK, "decision: {decision}");
    let jobs: Vec<(String, String)> =
        sqlx::query_as("SELECT kind, payload FROM jobs WHERE kind = 'lut_import'")
            .fetch_all(&server.db)
            .await
            .expect("jobs");
    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].1.contains(&id));

    let (status, _, body) = server.get(&format!("/api/proposals/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    let view: Value = serde_json::from_slice(&body).expect("view");
    assert_eq!(view["status"], "approved");
    assert_eq!(view["import"]["state"], "queued");
}

#[tokio::test(flavor = "multi_thread")]
async fn lut_submission_rejects_bad_bytes_and_duplicate_links() {
    const SCHEMA: &str = include_str!("../../../data/bodies/fujifilm-x-t5.yaml");
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let cube = lut_test_cube();
    let bytes = cube.as_bytes();
    let digest = hex(&Sha256::digest(bytes));
    let payload = |sha256: &str| {
        format!(
            "look: test-lut\nname: Test LUT\nlicense: {{ own_work: true }}\n\
             attribution: A contributor\nlut:\n  file: test-lut.cube\n  sha256: {sha256}\n  \
             size: {}\nencoding: flog2_fgamut\n",
            bytes.len()
        )
    };

    let (status, body) = server
        .submit_lut("lut_look", &payload(&"00".repeat(32)), bytes)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "digest: {body}");
    assert_eq!(body["error"], "SCHEMA_INVALID");

    let (status, body) = server.submit_lut("schema_change", SCHEMA, bytes).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "attachment: {body}");
    assert_eq!(body["error"], "SCHEMA_INVALID");

    let frame = frame_lut_proposal(br#"{"kind":"lut_look"}"#, bytes).expect("frame");
    let request = Request::builder()
        .method("POST")
        .uri("/api/proposals/lut")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(frame))
        .expect("request");
    let (status, _, _) = send(&server.app, request).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let (status, body) = server
        .submit_lut("lut_look", &payload(&digest), bytes)
        .await;
    assert_eq!(status, StatusCode::CREATED, "first: {body}");
    let (status, body) = server
        .submit_lut("lut_look", &payload(&digest), bytes)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "duplicate: {body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn lut_submission_admits_declared_body_and_family_bases() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let cube = lut_test_cube();
    let bytes = cube.as_bytes();
    let digest = hex(&Sha256::digest(bytes));
    let payload = |look: &str, base: &str| {
        format!(
            "look: {look}\nname: Test LUT\nlicense: {{ own_work: true }}\n\
             attribution: A contributor\nlut:\n  file: test-lut.cube\n  sha256: {digest}\n  \
             size: {}\nencoding: !display\n  base:\n{base}",
            bytes.len()
        )
    };
    let body_base = payload(
        "body-base",
        "      kind: body_realization\n      body: fujifilm/x-t5\n      \
         renderer: x_raw_studio\n      film_simulation: classic_chrome\n      \
         dynamic_range: dr400\n",
    );
    let (status, body) = server.submit_lut("lut_look", &body_base, bytes).await;
    assert_eq!(status, StatusCode::CREATED, "body base: {body}");

    let inconsistent = payload(
        "inconsistent-base",
        "      kind: body_realization\n      body: fujifilm/x-t5\n      \
         renderer: x_raw_studio\n      film_simulation: no_such_sim\n      \
         dynamic_range: dr400\n",
    );
    let (status, body) = server.submit_lut("lut_look", &inconsistent, bytes).await;
    assert_eq!(status, StatusCode::CREATED, "inconsistent base: {body}");

    let family_base = payload(
        "family-base",
        "      kind: family_rendering\n      maker: Fujifilm\n      rendering: Classic Chrome\n",
    );
    let (status, body) = server.submit_lut("lut_look", &family_base, bytes).await;
    assert_eq!(status, StatusCode::CREATED, "family base: {body}");

    let unknown_body = payload(
        "unknown-body",
        "      kind: body_realization\n      body: fujifilm/no-such-body\n      renderer: x_raw_studio\n",
    );
    let (status, body) = server.submit_lut("lut_look", &unknown_body, bytes).await;
    assert_eq!(status, StatusCode::CREATED, "unknown body: {body}");
}

fn lut_test_cube() -> &'static str {
    "\
TITLE \"Test\"
LUT_3D_SIZE 2
DOMAIN_MIN 0.0 0.0 0.0
DOMAIN_MAX 1.0 1.0 1.0
0.0 0.0 0.0
1.0 0.0 0.0
0.0 1.0 0.0
1.0 1.0 0.0
0.0 0.0 1.0
1.0 0.0 1.0
0.0 1.0 1.0
1.0 1.0 1.0
"
}

#[tokio::test(flavor = "multi_thread")]
async fn rejecting_one_shared_lut_keeps_the_other_reference() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let bytes = lut_test_cube().as_bytes();
    let digest = hex(&Sha256::digest(bytes));
    let payload = |look: &str| {
        format!(
            "look: {look}\nname: {look}\nlicense: {{ own_work: true }}\n\
             attribution: A contributor\nlut:\n  file: shared.cube\n  sha256: {digest}\n  \
             size: {}\nencoding: flog2_fgamut\n",
            bytes.len()
        )
    };
    let (status, first) = server
        .submit_lut("lut_look", &payload("shared-a"), bytes)
        .await;
    assert_eq!(status, StatusCode::CREATED, "first: {first}");
    let (status, second) = server
        .submit_lut("lut_look", &payload("shared-b"), bytes)
        .await;
    assert_eq!(status, StatusCode::CREATED, "second: {second}");
    let first = first["view"]["id"].as_str().expect("first id").to_owned();
    let second = second["view"]["id"].as_str().expect("second id").to_owned();
    let token = server.moderator_token().await;
    let (status, body) = server
        .decide_with(&token, &first, "reject", "duplicate")
        .await;
    assert_eq!(status, StatusCode::OK, "reject: {body}");
    let first_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM lut_uploads WHERE proposal_id = ?")
            .bind(&first)
            .fetch_one(&server.db)
            .await
            .expect("first rows");
    assert_eq!(first_rows, 0);
    let second_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM lut_uploads WHERE proposal_id = ?")
            .bind(&second)
            .fetch_one(&server.db)
            .await
            .expect("second rows");
    assert_eq!(second_rows, 1);
    let referenced: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blob_references WHERE hash = ?")
        .bind(&digest)
        .fetch_one(&server.db)
        .await
        .expect("references");
    assert_eq!(referenced, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_credential_pending_lut_storage_is_bounded() {
    let mut server = TestServer::start().await;
    server.persist_credential().await;
    let credential: String =
        sqlx::query_scalar("SELECT id FROM credentials WHERE credential_id = ?")
            .bind(server.credential_id.as_slice())
            .fetch_one(&server.db)
            .await
            .expect("credential");
    for index in 0..2u8 {
        let id = format!("quota-{index}");
        sqlx::query(
            "INSERT INTO proposals
             (id, credential_id, type, payload, human_summary, evidence, status, created_at)
             VALUES (?, ?, 'lut_look', '{}', 'x', '[]', 'pending', 0)",
        )
        .bind(&id)
        .bind(&credential)
        .execute(&server.db)
        .await
        .expect("proposal");
        sqlx::query(
            "INSERT INTO lut_uploads
             (proposal_id, sha256, credential_id, file, size, created_at)
             VALUES (?, ?, ?, 'quota.cube', ?, 0)",
        )
        .bind(&id)
        .bind(vec![index; 32])
        .bind(&credential)
        .bind(32i64 * 1024 * 1024)
        .execute(&server.db)
        .await
        .expect("upload");
    }
    let bytes = lut_test_cube().as_bytes();
    let digest = hex(&Sha256::digest(bytes));
    let payload = format!(
        "look: quota-lut\nname: Quota LUT\nlicense: {{ own_work: true }}\n\
         attribution: A contributor\nlut:\n  file: quota.cube\n  sha256: {digest}\n  \
         size: {}\nencoding: flog2_fgamut\n",
        bytes.len()
    );
    let (status, body) = server.submit_lut("lut_look", &payload, bytes).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "quota: {body}");
    assert_eq!(body["error"], "SCHEMA_INVALID");
}

#[tokio::test]
async fn legacy_persistent_bounties_are_fenced_but_coverage_survives() {
    let server = TestServer::start().await;
    for (id, reason) in [
        (
            "legacy-persistent",
            r#"{ "kind": "persistent_residual", "gap": 1.0, "attribution": "unassessed" }"#,
        ),
        (
            "legacy-coverage",
            r#"{ "kind": "unverified_region", "sigma": 1.0 }"#,
        ),
    ] {
        sqlx::query(
            "INSERT INTO bounties (
                 id, target_look, look_version, body_id, region, metric, expected_variance_reduction,
                 reason, status, created_at
             ) VALUES (?, 'classic-chrome', 1, 'fujifilm/x-t4', 'h1/e1',
                       'looklab-scoring-metric/legacy-pre-clip/1', 1.0, ?, 'open', 0)",
        )
        .bind(id)
        .bind(reason)
        .execute(&server.db)
        .await
        .expect("legacy bounty");
    }
    let (status, _, bytes) = server.get("/bounties.json?body=fujifilm%2Fx-t4").await;
    assert_eq!(status, StatusCode::OK);
    let reply: serde_json::Value = serde_json::from_slice(&bytes).expect("bounties json");
    let bounties = reply["bounties"].as_array().expect("bounties");
    assert_eq!(bounties.len(), 1, "{reply}");
    assert_eq!(bounties[0]["reason"]["kind"], "unverified_region");
}

#[tokio::test]
async fn encoded_camera_filters_select_report_free_evidence_requests() {
    let server = TestServer::start().await;
    let mut conn = server.db.acquire().await.expect("connection");
    for body in ["fujifilm/x-t4", "sony/a7"] {
        let subject = looklab_schema::ModelSubject::BodyRealization {
            realization: looklab_schema::BodyRealizationId {
                body: looklab_schema::BodyId::new(body).expect("body"),
                epoch: looklab_schema::ControlEpochId::new(1),
                native_sim: looklab_schema::SimKey::new("provia").expect("sim"),
            },
        };
        let key = looklab_server::model_store::register_subject(&mut conn, &subject, 0)
            .await
            .expect("subject");
        looklab_server::model_store::record_active_evidence_request(
            &mut conn,
            &key,
            looklab_schema::QualificationCapability::NativePrediction,
            &looklab_schema::EvidenceRequest {
                kind: looklab_schema::EvidenceRequestKind::MissingIndependentCaptures,
                detail: "Add independent RAW captures with their in-camera JPEGs.".to_owned(),
            },
            Some(1),
            0,
        )
        .await
        .expect("request");
    }
    drop(conn);
    let request = Request::builder()
        .uri("/bounties.json?body=fujifilm%2Fx-t4")
        .body(Body::empty())
        .expect("request");
    let (status, _, bytes) = send(&server.app, request).await;
    assert_eq!(status, StatusCode::OK);
    #[derive(serde::Deserialize)]
    struct Reply {
        evidence_requests: Vec<looklab_schema::EvidenceRequestWire>,
    }
    let reply: Reply = serde_json::from_slice(&bytes).expect("reply");
    let bodies: Vec<_> = reply
        .evidence_requests
        .iter()
        .map(|request| request.body().map(|body| body.as_str()))
        .collect();
    assert_eq!(bodies, [Some("fujifilm/x-t4")]);
    assert!(matches!(
        reply.evidence_requests[0].origin,
        looklab_schema::EvidenceRequestOrigin::PreCandidate
    ));
    assert_eq!(reply.evidence_requests[0].observed_groups, Some(1));
}

#[tokio::test]
async fn unbound_legacy_recipe_settings_do_not_poison_current_authority() {
    let server = TestServer::start().await;
    let look = looklab_schema::LookId::new("upgrade-legacy").expect("look");
    let target_version = looklab_schema::TargetVersion::new(7).expect("version");
    let realization = looklab_schema::BodyRealizationId {
        body: looklab_schema::BodyId::new("fujifilm/x-t4").expect("body"),
        epoch: looklab_schema::ControlEpochId::new(1),
        native_sim: looklab_schema::SimKey::new("classic_chrome").expect("sim"),
    };
    let subject_key = looklab_schema::ModelSubject::BodyRealization {
        realization: realization.clone(),
    }
    .key();
    let version = looklab_schema::BodyRealizationVersionId {
        realization,
        version: looklab_schema::RealizationVersion::new(1).expect("realization version"),
        target: looklab_schema::FamilyTargetId {
            family: look.clone(),
            version: target_version,
        },
    };
    let revision = looklab_schema::RealizationRevisionId::for_version(&version);
    let target_key = format!("{}@7", look.as_str());
    let hash = "a".repeat(64);
    sqlx::query(
        "INSERT INTO look_versions (id, look, version, status, changelog, created_at)
         VALUES ('upgrade-legacy-v7', ?, 7, 'published', NULL, 0)",
    )
    .bind(look.as_str())
    .execute(&server.db)
    .await
    .expect("look version");
    sqlx::query(
        "INSERT INTO model_body_states (
             body, version, stage, profile_sha256, input_transform_sha256,
             body_params_sha256, gauge_sha256, created_at
         ) VALUES ('fujifilm/x-t4', 1, 'published', NULL, ?, ?, ?, 0)",
    )
    .bind(&hash)
    .bind(&hash)
    .bind(&hash)
    .execute(&server.db)
    .await
    .expect("body state");
    sqlx::query(
        "INSERT INTO model_subjects (key, kind, family, body, epoch, native_sim, created_at)
         VALUES (?, 'body_realization', NULL, 'fujifilm/x-t4', 1, 'classic_chrome', 0)",
    )
    .bind(subject_key.as_str())
    .execute(&server.db)
    .await
    .expect("subject");
    sqlx::query(
        "INSERT INTO model_realization_revisions (
             id, subject_key, body, version, family, target_version, body_state_version,
             input_transform_version, realization_params_version, control_response_version,
             clip_k, status, status_reason, numerical, release_gates, created_at
         ) VALUES (?, ?, 'fujifilm/x-t4', 1, 'upgrade-legacy', 7, 1, 1, 1, 1,
                   0.03, 'candidate', NULL, 'passed', 'passed', 0)",
    )
    .bind(revision.as_str())
    .bind(subject_key.as_str())
    .execute(&server.db)
    .await
    .expect("realization revision");
    sqlx::query(
        "INSERT INTO model_family_heads (
             family, head_version, source_limited, numerical, release_gates, report_id,
             assessed_generation, applicability, invalidated_reason, updated_at
         ) VALUES ('upgrade-legacy', 7, 0, 'passed', 'passed', NULL, 0, 'current', NULL, 0)",
    )
    .execute(&server.db)
    .await
    .expect("family head");
    sqlx::query(
        "INSERT INTO model_current_projections (
             subject_key, \"use\", capability, scope_key, selected_report_id,
             selected_candidate_kind, selected_candidate_id, selected_candidate_target,
             selected_candidate_version, pending_generation, applicability,
             invalidated_reason, updated_at
         ) VALUES (?, 'experimental_match', 'native_prediction', 'all', NULL,
                   'body_realization', ?, ?, 1, 0, 'current', NULL, 0)",
    )
    .bind(subject_key.as_str())
    .bind(revision.as_str())
    .bind(&target_key)
    .execute(&server.db)
    .await
    .expect("projection");
    sqlx::query(
        "INSERT INTO recipe_scores (
             id, body_id, epoch, base_sim, settings, target_look, metric, predicted_de,
             predicted_sigma, measured_de, measured_sigma, n_verifications,
             input_transform_version, control_response_version, look_version,
             source_realization, source_body_state, source_report, source_scope, updated_at
         ) VALUES (
             'current-recipe', 'fujifilm/x-t4', 1, 'classic_chrome', '[]',
             'upgrade-legacy', 'looklab-scoring-metric/rendered-post-clip/1', 1.0,
             NULL, 1.0, 0.5, 1, 1, 1, 7, ?, 1, NULL, 'experimental', 0
         )",
    )
    .bind(revision.as_str())
    .execute(&server.db)
    .await
    .expect("current score");
    sqlx::query(
        "INSERT INTO recipe_scores (
             id, body_id, epoch, base_sim, settings, target_look, predicted_de,
             predicted_sigma, measured_de, measured_sigma, n_verifications,
             input_transform_version, control_response_version, look_version,
             source_realization, source_body_state, source_report, source_scope, updated_at
         ) VALUES (
             'legacy-recipe', 'fujifilm/x-t4', 1, 'classic_chrome', '{}',
             'upgrade-legacy', 3.75, NULL, 4.0, 0.5, 2, 0, 0, 7, NULL, NULL, NULL, NULL, 0
         )",
    )
    .execute(&server.db)
    .await
    .expect("legacy score");

    let (status, _, bytes) = server
        .get("/api/recipes?body=fujifilm%2Fx-t4&look=upgrade-legacy")
        .await;
    assert_eq!(status, StatusCode::OK);
    let reply: serde_json::Value = serde_json::from_slice(&bytes).expect("recipes json");
    assert_eq!(reply.as_array().map(Vec::len), Some(1), "{reply}");
    assert_eq!(reply[0]["entries"][0]["model_versions"]["look"], 7);

    sqlx::query(
        "UPDATE model_family_heads
         SET applicability = 'invalidated', invalidated_reason = 'withdrawal'
         WHERE family = 'upgrade-legacy'",
    )
    .execute(&server.db)
    .await
    .expect("invalidate head");
    let (status, _, bytes) = server
        .get("/api/recipes?body=fujifilm%2Fx-t4&look=upgrade-legacy")
        .await;
    assert_eq!(status, StatusCode::OK);
    let reply: serde_json::Value = serde_json::from_slice(&bytes).expect("recipes json");
    assert_eq!(reply, serde_json::json!([]));
}
