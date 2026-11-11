// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::PathBuf;

use looklab_publish::{
    ArtifactHash, PublishError, Scene, SceneCredit, SceneIndex, SceneKind, SceneName, SiteIndex,
    write_scene_sources,
};
use looklab_worker::WorkerError;
use looklab_worker::publish::{PublishConfig, publish_with_pool};
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    data: PathBuf,
    site: PathBuf,
    blobs: PathBuf,
    transforms: PathBuf,
    db: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().expect("temp dir");
        let data = dir.path().join("data");
        std::fs::create_dir_all(data.join("bodies")).expect("bodies");
        std::fs::create_dir_all(data.join("card/conformance-v1")).expect("conformance");
        let blobs = dir.path().join("blobs");
        std::fs::create_dir_all(&blobs).expect("blobs");
        let transforms = dir.path().join("transforms");
        std::fs::create_dir_all(&transforms).expect("transforms");
        let site = dir.path().join("site");
        let db = dir.path().join("looklab.sqlite3");
        Self {
            _dir: dir,
            data,
            site,
            blobs,
            transforms,
            db,
        }
    }

    async fn publish(&self) -> Result<(), WorkerError> {
        let pool = looklab_server::open_database(&self.db)
            .await
            .expect("database");
        let config = PublishConfig {
            db: self.db.clone(),
            blobs: self.blobs.clone(),
            data: self.data.clone(),
            manuals: self.data.join("manuals"),
            snapshot_dir: self.data.join("community"),
            transforms: self.transforms.clone(),
            out: self.site.clone(),
        };
        publish_with_pool(&pool, &config).await.map(|_| ())
    }
}

fn photo(index: usize) -> Scene {
    let mut scene = Scene::new(
        SceneName::new(format!("photo-{index}")).expect("scene name"),
        8,
        6,
        vec![[0.18, 0.18, 0.18]; 48],
    )
    .expect("scene");
    scene.kind = SceneKind::Photo;
    scene.title = format!("Scene {index}");
    scene.credit = Some(SceneCredit {
        text: format!("Camera {index} · raw.pixls.us · CC0"),
        source_url: format!("https://example.com/scene-{index}.dng"),
        license: "CC0-1.0".to_owned(),
        source_sha256: ArtifactHash::of(format!("source-{index}").as_bytes()),
    });
    scene
}

fn write_community_snapshot(fixture: &Fixture, version: u64) {
    let root = fixture.data.join(format!("community/v{version}"));
    std::fs::create_dir_all(root.join("bodies")).expect("snapshot bodies");
    let shipped = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/bodies");
    let body_text = std::fs::read_to_string(shipped.join("fujifilm-x-t5.body.yaml")).expect("body");
    let schema_text = std::fs::read_to_string(shipped.join("fujifilm-x-t5.yaml")).expect("schema");
    let body = looklab_schema::validate_body(&body_text).expect("body");
    let schema = looklab_schema::validate_control_schema(&schema_text).expect("schema");
    std::fs::write(
        root.join(looklab_schema::body_export_path(&body.id)),
        body_text,
    )
    .expect("snapshot body");
    std::fs::write(
        root.join(looklab_schema::schema_export_path(&schema.body)),
        schema_text,
    )
    .expect("snapshot schema");
}

fn prepare(fixture: &Fixture, count: usize) -> Vec<Scene> {
    let scenes = (0..count).map(photo).collect::<Vec<_>>();
    write_scene_sources(&fixture.data.join("scenes"), &scenes).expect("scene sources");
    scenes
}

fn index(fixture: &Fixture, name: &str) -> Vec<u8> {
    std::fs::read(fixture.site.join("data").join(name)).expect("published file")
}

#[tokio::test]
async fn publication_lists_prepared_photo_scenes_with_their_provenance() {
    let fixture = Fixture::new();
    let prepared = prepare(&fixture, 5);
    fixture.publish().await.expect("publish");

    let scenes: SceneIndex =
        serde_json::from_slice(&index(&fixture, "scenes/index.json")).expect("scene index");
    let site: SiteIndex =
        serde_json::from_slice(&index(&fixture, "index.json")).expect("site index");
    assert_eq!(site.scenes, scenes.scenes);

    let photos = scenes
        .scenes
        .iter()
        .filter(|scene| scene.kind == SceneKind::Photo)
        .collect::<Vec<_>>();
    assert!(
        (4..=6).contains(&photos.len()),
        "{} photo scenes",
        photos.len()
    );
    assert_eq!(photos.len(), prepared.len());
    assert_eq!(
        scenes
            .scenes
            .iter()
            .filter(|scene| scene.kind == SceneKind::TestChart)
            .count(),
        3
    );

    for (summary, source) in photos.iter().zip(&prepared) {
        assert_eq!(summary.name, source.name);
        assert_eq!(summary.title, source.title);
        assert_eq!(summary.credit, source.credit);
        assert_eq!((summary.width, summary.height), (8, 6));
        let png = std::fs::read(fixture.site.join(summary.path.as_str())).expect("scene png");
        assert_eq!(summary.sha256, ArtifactHash::of(&png));
        assert_eq!(summary.bytes, png.len() as u64);
        let decoded = Scene::from_png(summary.name.clone(), &png).expect("decode scene");
        assert_eq!((decoded.width, decoded.height), (8, 6));
        let thumbnail =
            std::fs::read(fixture.site.join(summary.thumbnail.path.as_str())).expect("thumbnail");
        assert_eq!(summary.thumbnail.sha256, ArtifactHash::of(&thumbnail));
    }
}

#[tokio::test]
async fn publication_uses_the_newest_approved_community_snapshot() {
    let fixture = Fixture::new();
    prepare(&fixture, 1);
    write_community_snapshot(&fixture, 3);
    fixture.publish().await.expect("publish");

    let site: SiteIndex =
        serde_json::from_slice(&index(&fixture, "index.json")).expect("site index");
    assert_eq!(site.community_snapshot_version, 3);
    assert!(site.looks.is_empty(), "no Look was fitted");
    assert_eq!(site.bodies.len(), 1);
    assert_eq!(site.bodies[0].id.as_str(), "fujifilm/x-t5");
}

#[tokio::test]
async fn publication_rejects_scene_bytes_that_do_not_match_the_prepared_index() {
    let fixture = Fixture::new();
    prepare(&fixture, 4);
    let source = fixture.data.join("scenes").join("photo-1.png");
    let mut png = std::fs::read(&source).expect("prepared png");
    png.push(0);
    std::fs::write(&source, png).expect("corrupt png");

    let error = fixture.publish().await.expect_err("mismatched scene");
    assert!(
        matches!(error, WorkerError::Publish(PublishError::SceneSources(_))),
        "{error}"
    );
    assert!(!fixture.site.join("data/index.json").is_file());
}

#[tokio::test]
async fn publication_without_a_prepared_scene_directory_publishes_test_charts() {
    let fixture = Fixture::new();
    fixture.publish().await.expect("publish");

    let scenes: SceneIndex =
        serde_json::from_slice(&index(&fixture, "scenes/index.json")).expect("scene index");
    assert_eq!(scenes.scenes.len(), 3);
    assert!(
        scenes
            .scenes
            .iter()
            .all(|scene| scene.kind == SceneKind::TestChart)
    );
}
