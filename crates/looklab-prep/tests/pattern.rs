// SPDX-License-Identifier: AGPL-3.0-or-later

mod common;

use looklab_color::RgbSpace;
use looklab_prep::Image3;
use looklab_prep::geom::{Point, Quad};
use looklab_prep::pattern::{self, PatternError, PatternParams, PatternSeed};
use rand_chacha::ChaCha20Rng;
use sha2::{Digest, Sha256};

const PATTERN_WIDTH: u32 = 378;
const PATTERN_HEIGHT: u32 = 672;
const PHOTO_WIDTH: u32 = 432;
const PHOTO_HEIGHT: u32 = 768;

struct Display {
    gamma: f64,
    tint: [f64; 3],
    brightness: f64,
}

const DISPLAYS: [Display; 3] = [
    Display {
        gamma: 2.2,
        tint: [1.0, 1.0, 1.0],
        brightness: 1.0,
    },
    Display {
        gamma: 2.4,
        tint: [1.06, 1.0, 0.9],
        brightness: 0.8,
    },
    Display {
        gamma: 2.0,
        tint: [0.94, 1.0, 1.07],
        brightness: 0.9,
    },
];

const EXPOSURES: [f64; 3] = [1.0, 0.5, 0.25];

fn shown(pattern: &Image3, display: &Display) -> Image3 {
    let data = pattern
        .data
        .iter()
        .map(|rgb| {
            let mut out = [0.0f32; 3];
            for channel in 0..3 {
                let light = common::display_gamma(f64::from(rgb[channel]), display.gamma);
                out[channel] = (light * display.tint[channel] * display.brightness) as f32;
            }
            out
        })
        .collect();
    Image3::new(pattern.width, pattern.height, data).expect("dimensions match")
}

fn pose(rng: &mut ChaCha20Rng) -> Quad {
    let height = (0.62 + 0.16 * common::uniform(rng)) * f64::from(PHOTO_HEIGHT);
    let width = height * 9.0 / 16.0;
    let centre_x =
        f64::from(PHOTO_WIDTH) * 0.5 + common::signed(rng, 0.02 * f64::from(PHOTO_WIDTH));
    let centre_y =
        f64::from(PHOTO_HEIGHT) * 0.5 + common::signed(rng, 0.02 * f64::from(PHOTO_HEIGHT));
    let angle = common::signed(rng, 0.24);
    let (sin, cos) = (libm::sin(angle), libm::cos(angle));
    let half_width = width * 0.5;
    let half_height = height * 0.5;
    let base = [
        Point::new(-half_width, -half_height),
        Point::new(half_width, -half_height),
        Point::new(half_width, half_height),
        Point::new(-half_width, half_height),
    ];
    let quad: Quad = base.map(|point| {
        Point::new(
            centre_x + cos * point.x - sin * point.y,
            centre_y + sin * point.x + cos * point.y,
        )
    });
    common::jitter_quad(&quad, 2.5, rng)
}

fn seed(fill: u8) -> PatternSeed {
    PatternSeed::from_bytes([fill; 32])
}

#[test]
fn a_clean_render_detects_every_patch_in_seed_order() {
    let seed = seed(0x21);
    let pattern = pattern::render(&seed, PATTERN_WIDTH, PATTERN_HEIGHT).expect("renders");
    let params = PatternParams::default();
    let mut poses = Vec::new();
    let mut rng = common::rng(0x99);
    for _ in 0..2 {
        poses.push(pose(&mut rng));
    }

    let mut tables = Vec::new();
    for pose in &poses {
        let scene = common::warp(&pattern, pose, PHOTO_WIDTH, PHOTO_HEIGHT, [0.0, 0.0, 0.0]);
        let jpeg = common::jpeg_from(scene.clone());
        let detection =
            pattern::detect(&seed, &jpeg, &scene, &common::identity_alignment(), &params)
                .expect("detects its own render");

        assert_eq!(detection.seed_digest, seed.digest());
        assert_eq!(detection.patches.rows().len(), pattern::PATCH_COUNT);
        let mut previous = None;
        for row in detection.patches.rows() {
            if let Some(previous) = previous {
                assert!(row.id.get() > previous);
            }
            previous = Some(row.id.get());
        }
        tables.push(detection.patches);
    }

    for (first, second) in tables[0].rows().iter().zip(tables[1].rows()) {
        assert_eq!(first.id, second.id);
        for channel in 0..3 {
            let a = f64::from(first.out_mean[channel]);
            let b = f64::from(second.out_mean[channel]);
            assert!(
                (a - b).abs() <= 0.02 * a.max(0.05),
                "id {} channel {channel}: {a} against {b}",
                first.id.get()
            );
        }
    }
}

#[test]
fn another_session_seed_is_rejected() {
    let session = seed(0x31);
    let other = seed(0x32);
    let pattern = pattern::render(&session, PATTERN_WIDTH, PATTERN_HEIGHT).expect("renders");
    let scene = common::warp(
        &pattern,
        &pose(&mut common::rng(0xab)),
        PHOTO_WIDTH,
        PHOTO_HEIGHT,
        [0.0, 0.0, 0.0],
    );
    let jpeg = common::jpeg_from(scene.clone());
    let result = pattern::detect(
        &other,
        &jpeg,
        &scene,
        &common::identity_alignment(),
        &PatternParams::default(),
    );
    assert!(matches!(
        result,
        Err(PatternError::SessionMismatch { found, expected })
            if found == session.digest() && expected == other.digest()
    ));
}

#[test]
fn a_plain_scene_has_no_fiducials() {
    let scene = common::blank(PHOTO_WIDTH, PHOTO_HEIGHT, [0.4, 0.45, 0.5]);
    let jpeg = common::jpeg_from(scene.clone());
    assert!(matches!(
        pattern::detect(
            &seed(0x41),
            &jpeg,
            &scene,
            &common::identity_alignment(),
            &PatternParams::default(),
        ),
        Err(PatternError::FiducialsNotFound)
    ));
}

#[test]
fn detection_rate_clears_the_phone_pattern_target() {
    let seed = seed(0x51);
    let pattern = pattern::render(&seed, PATTERN_WIDTH, PATTERN_HEIGHT).expect("renders");
    let params = PatternParams::default();
    let mut rng = common::rng(0x1234);
    let mut attempts = 0;
    let mut detected = 0;
    for display in &DISPLAYS {
        let shown = shown(&pattern, display);
        for exposure in EXPOSURES {
            for _ in 0..3 {
                attempts += 1;
                let scene = common::warp(
                    &shown,
                    &pose(&mut rng),
                    PHOTO_WIDTH,
                    PHOTO_HEIGHT,
                    [0.01, 0.01, 0.012],
                );
                let scene = common::scale(&scene, exposure);
                let scene = common::blur(&scene, 2);
                let scene = common::add_noise(&scene, 0.0015, &mut rng);
                let jpeg = common::jpeg_from(scene.clone());
                if pattern::detect(&seed, &jpeg, &scene, &common::identity_alignment(), &params)
                    .is_ok()
                {
                    detected += 1;
                }
            }
        }
    }
    assert!(
        detected * 100 >= 95 * attempts,
        "detected {detected} of {attempts}"
    );
}

fn fixture_seed() -> PatternSeed {
    let digest = Sha256::digest(b"looklab-fixture-pattern-v1");
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&digest);
    PatternSeed::from_bytes(bytes)
}

fn fixture_content() -> Image3 {
    let bytes = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/pattern/pattern.png"
    ));
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes.as_slice()));
    let mut reader = decoder.read_info().expect("the fixture is a PNG");
    let size = reader
        .output_buffer_size()
        .expect("the fixture fits memory");
    let mut buffer = vec![0u8; size];
    let info = reader.next_frame(&mut buffer).expect("the fixture decodes");
    assert_eq!(info.color_type, png::ColorType::Rgb);
    assert_eq!(info.bit_depth, png::BitDepth::Eight);

    let data: Vec<[f32; 3]> = buffer[..info.buffer_size()]
        .as_chunks::<3>()
        .0
        .iter()
        .map(|pixel| {
            [
                RgbSpace::Srgb.decode(f64::from(pixel[0]) / 255.0) as f32,
                RgbSpace::Srgb.decode(f64::from(pixel[1]) / 255.0) as f32,
                RgbSpace::Srgb.decode(f64::from(pixel[2]) / 255.0) as f32,
            ]
        })
        .collect();
    let image = Image3::new(info.width, info.height, data).expect("dimensions match the buffer");

    let layout = pattern::PatternLayout::new(image.width, image.height).expect("fixture layout");
    let x0 = layout.content.x.round().max(0.0) as u32;
    let y0 = layout.content.y.round().max(0.0) as u32;
    let width = layout.content.width.round() as u32;
    let height = layout.content.height.round() as u32;
    let mut cropped = Vec::with_capacity((width * height) as usize);
    for y in y0..(y0 + height) {
        for x in x0..(x0 + width) {
            cropped.push(image.data[(y * image.width + x) as usize]);
        }
    }
    Image3::new(width, height, cropped).expect("crop dimensions match")
}

#[test]
fn the_committed_fixture_detects_with_its_published_seed() {
    let seed = fixture_seed();
    let content = fixture_content();
    assert_eq!(content.width, 810);
    let scene = common::warp_fine(
        &content,
        &pose(&mut common::rng(0x7171)),
        PHOTO_WIDTH,
        PHOTO_HEIGHT,
        [0.0, 0.0, 0.0],
    );
    let jpeg = common::jpeg_from(scene.clone());
    let detection = pattern::detect(
        &seed,
        &jpeg,
        &scene,
        &common::identity_alignment(),
        &PatternParams::default(),
    )
    .expect("the fixture detects");
    assert_eq!(detection.seed_digest, seed.digest());
    assert_eq!(detection.patches.rows().len(), pattern::PATCH_COUNT);
}
