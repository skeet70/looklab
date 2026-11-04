// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end tests of the axum app against an in-process router, a temporary
//! SQLite file and a temporary blob directory.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use looklab_samples::{
    InputSpace, OutputSpace, PatchId, PatchRow, PatchTable, PatchTableHeader, PrepParamsHash,
    PrepVersion, PriorTransformVersion, RerenderGroupId, SampleFlags, SampleHeader, SampleSet,
};
use looklab_server::{AppState, Config, PowConfig, router};
use looklab_upload::{
    BatchManifest, BlobKind, BodyId, Challenge, ContributionKind, ControlEntry, ControlKey,
    ControlSettings, EnvelopeParts, FirmwareVersionString, Intent, Limits, ManifestHash,
    ManifestImage, ManifestMetadata, Nonce, PowParams, PowSolutions, RawSerial, SerialHmacKey,
    SimKey, StepIndex, WebAuthnAssertion, encode_envelope, manifest_hash, solve,
};
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
        .with_max_level(tracing::Level::TRACE)
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
    db: SqlitePool,
    blob_dir: PathBuf,
    soft: SoftPasskey,
    credential_id: Vec<u8>,
}

impl TestServer {
    async fn start() -> Self {
        let _ = logs();
        let dir = tempfile::tempdir().expect("temp dir");
        let secret = dir.path().join("server-secret");
        std::fs::write(&secret, [9u8; 32]).expect("write secret");
        let config = Config {
            bind: "127.0.0.1:0".parse().expect("bind"),
            db_path: dir.path().join("looklab.sqlite3"),
            blob_dir: dir.path().join("blobs"),
            rp_id: RP_ID.to_owned(),
            origin: ORIGIN.to_owned(),
            server_secret_path: secret,
            bodies_dir: Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/bodies"),
            nonce_ttl: Duration::from_secs(300),
            registration_ttl: Duration::from_secs(300),
            blob_retention_days: 30,
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
            },
        };
        let db_path = config.db_path.clone();
        let blob_dir = config.blob_dir.clone();
        let state = Arc::new(AppState::new(config).await.expect("state"));
        let app = router(state);
        let db = SqlitePool::connect_with(SqliteConnectOptions::new().filename(&db_path))
            .await
            .expect("test db");
        let mut soft = SoftPasskey::new(true);
        let credential_id = register(&app, &mut soft).await;
        Self {
            dir,
            app,
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
        let options = request_options(challenge.as_bytes(), &self.credential_id).public_key;
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

    async fn upload_bytes(&self, bytes: Vec<u8>) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("POST")
            .uri("/api/upload")
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from(bytes))
            .expect("request");
        let (status, _, body) = send(&self.app, request).await;
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    async fn upload_valid(&mut self, serial: Option<&RawSerial>) -> (StatusCode, Value) {
        let (manifest, blobs) = manifest(valid_settings());
        let nonce = self.nonce().await;
        let challenge = Challenge::new(&nonce, &manifest_hash(&manifest));
        let assertion = self.assertion(&challenge).await;
        let pow = solve(
            &challenge,
            u32::try_from(manifest.images.len()).expect("image count"),
            &pow_params(),
            0,
        )
        .expect("proof-of-work");
        let bytes = encode_envelope(&EnvelopeParts {
            nonce,
            manifest: &manifest,
            assertion: &assertion,
            pow: &pow,
            serial,
            blobs: &blobs,
        });
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

async fn register(app: &Router, soft: &mut SoftPasskey) -> Vec<u8> {
    let (status, body) = post_json(
        app,
        "/api/passkey/register/start",
        &json!({ "display_handle": "tester" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "register start: {body}");
    let handle = body["handle"].as_str().expect("handle").to_owned();
    let options: CreationChallengeResponse =
        serde_json::from_value(body["options"].clone()).expect("creation options");
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

fn request_options(challenge: &[u8], credential_id: &[u8]) -> RequestChallengeResponse {
    serde_json::from_value(json!({
        "publicKey": {
            "challenge": URL_SAFE_NO_PAD.encode(challenge),
            "rpId": RP_ID,
            "allowCredentials": [{
                "type": "public-key",
                "id": URL_SAFE_NO_PAD.encode(credential_id),
            }],
            "userVerification": "discouraged",
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

fn entry(key: &str, step: i64) -> ControlEntry {
    ControlEntry {
        key: ControlKey::new(key).expect("control key"),
        step: StepIndex::new(step),
    }
}

fn valid_settings() -> ControlSettings {
    ControlSettings::new(vec![
        entry("color", 0),
        entry("color_chrome_effect", 0),
        entry("color_chrome_fx_blue", 0),
        entry("dynamic_range", 0),
        entry("highlight_tone", 0),
        entry("shadow_tone", 0),
        entry("wb_shift_b", 0),
        entry("wb_shift_r", 0),
    ])
    .expect("settings")
}

fn manifest(settings: ControlSettings) -> (BatchManifest, Vec<Vec<u8>>) {
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
        body: BodyId::new("fujifilm/x-t5").expect("body id"),
        firmware_version: FirmwareVersionString::new("2.10").expect("firmware"),
        sim_key: SimKey::new("classic_chrome").expect("sim key"),
        settings,
        images,
    };
    (manifest, blobs)
}

fn envelope(
    nonce: Nonce,
    manifest: &BatchManifest,
    blobs: &[Vec<u8>],
    serial: Option<&RawSerial>,
    assertion: &WebAuthnAssertion,
    pow: &PowSolutions,
) -> Vec<u8> {
    encode_envelope(&EnvelopeParts {
        nonce,
        manifest,
        assertion,
        pow,
        serial,
        blobs,
    })
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

#[derive(Clone, Copy)]
enum RequestDomain {
    StatusReissue,
    Withdraw,
}

impl RequestDomain {
    const fn as_str(self) -> &'static str {
        match self {
            Self::StatusReissue => "status-reissue",
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
    assert_eq!(
        feed["contributions"]
            .as_array()
            .expect("contributions")
            .len(),
        1
    );
    let contribution = &feed["contributions"][0];
    assert_eq!(contribution["id"], contribution_id);
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
    assert!(rss.contains(contribution_id), "rss: {rss}");

    let credentials: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM credentials")
        .fetch_one(&server.db)
        .await
        .expect("credentials");
    assert_eq!(credentials, 1);
    let jobs: (String, i64) = sqlx::query_as("SELECT kind, COUNT(*) FROM jobs GROUP BY kind")
        .fetch_one(&server.db)
        .await
        .expect("jobs");
    assert_eq!(jobs.0, "consistency_score");
    assert_eq!(jobs.1, 1);
    let blobs = std::fs::read_dir(&server.blob_dir)
        .expect("blob dir")
        .count();
    assert_eq!(blobs, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_signature_is_rejected() {
    let mut server = TestServer::start().await;
    let (manifest, blobs) = manifest(valid_settings());
    let nonce = server.nonce().await;
    let challenge = Challenge::new(&nonce, &manifest_hash(&manifest));
    let mut assertion = server.assertion(&challenge).await;
    assertion.signature[0] ^= 0xff;
    let pow = solve(&challenge, 2, &pow_params(), 0).expect("pow");
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
    let challenge = Challenge::new(&nonce, &manifest_hash(&manifest));
    let assertion = server.assertion(&challenge).await;
    let pow = solve(&challenge, 2, &pow_params(), 0).expect("pow");
    let bytes = envelope(nonce, &manifest, &blobs, None, &assertion, &pow);
    let (status, _) = server.upload_bytes(bytes.clone()).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = server.upload_bytes(bytes).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "NONCE_REPLAYED");
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_pow_is_rejected() {
    let mut server = TestServer::start().await;
    let (manifest, blobs) = manifest(valid_settings());
    let nonce = server.nonce().await;
    let challenge = Challenge::new(&nonce, &manifest_hash(&manifest));
    let assertion = server.assertion(&challenge).await;
    let pow = bad_pow(&challenge, 2);
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
    let challenge = Challenge::new(&nonce, &manifest_hash(&manifest));
    let assertion = server.assertion(&challenge).await;
    let pow = solve(&challenge, 2, &pow_params(), 0).expect("pow");
    let mut bytes = envelope(nonce, &manifest, &blobs, None, &assertion, &pow);
    bytes.push(0);
    let (status, body) = server.upload_bytes(bytes).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "MALFORMED_ENVELOPE");
}

#[tokio::test(flavor = "multi_thread")]
async fn schema_invalid_settings_are_rejected() {
    let mut server = TestServer::start().await;
    let settings = ControlSettings::new(vec![entry("highlight_tone", 0)]).expect("settings");
    let (manifest, blobs) = manifest(settings);
    let nonce = server.nonce().await;
    let challenge = Challenge::new(&nonce, &manifest_hash(&manifest));
    let assertion = server.assertion(&challenge).await;
    let pow = solve(&challenge, 2, &pow_params(), 0).expect("pow");
    let bytes = envelope(nonce, &manifest, &blobs, None, &assertion, &pow);
    let (status, body) = server.upload_bytes(bytes).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "SCHEMA_INVALID");
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
        "contribution_id": contribution_id,
    });
    let (status, body) = post_json(&server.app, "/api/withdraw", &payload).await;
    assert_eq!(status, StatusCode::OK, "withdraw: {body}");
    let delete_after = body["delete_after"].as_i64().expect("delete_after");

    let feed: Value = serde_json::from_slice(&server.get(&format!("/status/{token}")).await.2)
        .expect("status json");
    assert_eq!(feed["contributions"][0]["status"], "withdrawn");

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
async fn status_reissue_revokes_the_old_token() {
    let mut server = TestServer::start().await;
    let (status, body) = server.upload_valid(None).await;
    assert_eq!(status, StatusCode::CREATED);
    let old_token = body["status_token"].as_str().expect("token").to_owned();

    let nonce = server.nonce().await;
    let challenge = Challenge::new(&nonce, &request_digest(RequestDomain::StatusReissue, ""));
    let assertion = server.assertion(&challenge).await;
    let payload = json!({
        "nonce": b64(nonce.as_bytes()),
        "credential_id": b64(&assertion.credential_id),
        "authenticator_data": b64(&assertion.authenticator_data),
        "client_data_json": b64(&assertion.client_data_json),
        "signature": b64(&assertion.signature),
    });
    let (status, body) = post_json(&server.app, "/api/status/reissue", &payload).await;
    assert_eq!(status, StatusCode::OK, "reissue: {body}");
    let new_token = body["status_token"].as_str().expect("token").to_owned();
    assert_ne!(new_token, old_token);

    let (status, _, _) = server.get(&format!("/status/{old_token}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = server.get(&format!("/status/{new_token}")).await;
    assert_eq!(status, StatusCode::OK);
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
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_sensitive_reaches_disk_or_logs() {
    let mut server = TestServer::start().await;
    let serial = RawSerial::new(SERIAL.as_bytes()).expect("serial");

    let (status, body) = server.upload_valid(Some(&serial)).await;
    assert_eq!(status, StatusCode::CREATED, "upload: {body}");

    let (manifest, blobs) = manifest(valid_settings());
    let nonce = server.nonce().await;
    let challenge = Challenge::new(&nonce, &manifest_hash(&manifest));
    let mut assertion = server.assertion(&challenge).await;
    assertion.signature[0] ^= 0xff;
    let pow = solve(&challenge, 2, &pow_params(), 0).expect("pow");
    let (status, _) = server
        .upload_bytes(envelope(nonce, &manifest, &blobs, None, &assertion, &pow))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let nonce = server.nonce().await;
    let challenge = Challenge::new(&nonce, &manifest_hash(&manifest));
    let assertion = server.assertion(&challenge).await;
    let pow = bad_pow(&challenge, 2);
    let (status, _) = server
        .upload_bytes(envelope(nonce, &manifest, &blobs, None, &assertion, &pow))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let hmac = serial.hmac(&SerialHmacKey::new([9; 32]), "Fujifilm", "X-T5");
    let hmac_bytes = hmac.as_bytes().to_vec();
    let hmac_hex = hex(&hmac_bytes);

    let credential_json: String = sqlx::query_scalar("SELECT webauthn_credential FROM credentials")
        .fetch_one(&server.db)
        .await
        .expect("credential");
    let passkey: webauthn_rs::prelude::Passkey =
        serde_json::from_str(&credential_json).expect("stored passkey");
    let needles = public_key_needles(&passkey);
    assert!(
        needles
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

    let log_bytes = log_bytes();
    assert!(!log_bytes.is_empty(), "no log output was captured");
    let log_text = String::from_utf8_lossy(&log_bytes).into_owned();
    assert!(!log_text.contains(SERIAL), "raw serial in logs");
    assert!(!log_text.contains(&hmac_hex), "serial HMAC in logs");
    for needle in &needles {
        assert!(!contains(&log_bytes, needle), "public key material in logs");
        assert!(
            !log_text.contains(String::from_utf8_lossy(needle).as_ref()),
            "public key text in logs"
        );
    }

    let db_files = database_files(server.dir.path());
    assert!(!db_files.is_empty());
    for file in &db_files {
        let bytes = std::fs::read(file).expect("read database file");
        assert!(
            !bytes.contains_subslice(SERIAL.as_bytes()),
            "serial in database"
        );
        for needle in needles.iter().take(3) {
            assert!(!contains(&bytes, needle), "public key bytes in database");
        }
    }
    for run in text_runs(&database_bytes(&db_files)) {
        assert!(!contains_ipv4(&run), "IPv4 address in database: {run}");
        assert!(!contains_ipv6(&run), "IPv6 address in database: {run}");
    }

    for entry in std::fs::read_dir(&server.blob_dir).expect("blob dir") {
        let path = entry.expect("entry").path();
        let bytes = std::fs::read(&path).expect("blob");
        assert!(
            !bytes.contains_subslice(SERIAL.as_bytes()),
            "serial in blob"
        );
        for needle in &needles {
            assert!(
                !contains(bytes.as_slice(), needle),
                "public key material in blob"
            );
        }
        for run in text_runs(&bytes) {
            assert!(!contains_ipv4(&run), "IPv4 address in blob: {run}");
            assert!(!contains_ipv6(&run), "IPv6 address in blob: {run}");
        }
    }

    for (table, column, value) in column_values(&server.db).await {
        if table == "credentials" && column == "webauthn_credential" {
            continue;
        }
        for needle in &needles {
            assert!(
                !contains(&value, needle),
                "public key material in {table}.{column}"
            );
        }
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

trait ContainsSubslice {
    fn contains_subslice(&self, needle: &[u8]) -> bool;
}

impl ContainsSubslice for [u8] {
    fn contains_subslice(&self, needle: &[u8]) -> bool {
        contains(self, needle)
    }
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
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let starts_hex = bytes[index].is_ascii_hexdigit() || bytes[index] == b':';
        if !starts_hex {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len()
            && (bytes[index].is_ascii_hexdigit() || bytes[index] == b':' || bytes[index] == b'.')
        {
            index += 1;
        }
        let token = &text[start..index];
        if (token.matches(':').count() >= 3 || token.contains("::")) && valid_ipv6(token) {
            return true;
        }
    }
    false
}

fn valid_ipv6(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let (head, tail) = match token.split_once("::") {
        Some((head, tail)) => {
            if tail.contains("::") {
                return false;
            }
            (head, Some(tail))
        }
        None => (token, None),
    };
    let head_groups: Vec<&str> = if head.is_empty() {
        Vec::new()
    } else {
        head.split(':').collect()
    };
    let tail_groups: Vec<&str> = match tail {
        Some("") => Vec::new(),
        Some(tail) => tail.split(':').collect(),
        None => Vec::new(),
    };
    let groups: Vec<&str> = head_groups.into_iter().chain(tail_groups).collect();
    let total = groups.len() + usize::from(tail.is_some());
    if total > 8 {
        return false;
    }
    groups.iter().all(|group| {
        !group.is_empty() && group.len() <= 4 && group.bytes().all(|byte| byte.is_ascii_hexdigit())
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
