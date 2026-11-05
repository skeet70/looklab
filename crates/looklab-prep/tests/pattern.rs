// SPDX-License-Identifier: AGPL-3.0-or-later

mod common;

use looklab_color::RgbSpace;
use looklab_prep::Image3;
use looklab_prep::geom::{Point, Quad};
use looklab_prep::pattern::{self, PATCH_COUNT, PatternError, PatternParams, PatternSeed};
use rand_chacha::ChaCha20Rng;
use sha2::{Digest, Sha256};

const PORTRAIT_PATTERN: (u32, u32) = (378, 819);
const LANDSCAPE_PATTERN: (u32, u32) = (672, 420);
const PORTRAIT_PHOTO: (u32, u32) = (432, 768);
const LANDSCAPE_PHOTO: (u32, u32) = (768, 432);

const PORTRAIT_PNG: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/pattern/pattern-portrait.png"
));
const LANDSCAPE_PNG: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/pattern/pattern-landscape.png"
));

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

#[derive(Clone, Copy)]
struct Shot {
    pattern: (u32, u32),
    photo: (u32, u32),
}

const SHOTS: [Shot; 2] = [
    Shot {
        pattern: PORTRAIT_PATTERN,
        photo: PORTRAIT_PHOTO,
    },
    Shot {
        pattern: LANDSCAPE_PATTERN,
        photo: LANDSCAPE_PHOTO,
    },
];

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

/// Clockwise quarter turns.
fn rotate(image: &Image3, quarter_turns: u8) -> Image3 {
    let mut current = image.clone();
    for _ in 0..quarter_turns % 4 {
        let width = current.height;
        let height = current.width;
        let mut data = vec![[0.0f32; 3]; (width * height) as usize];
        for y in 0..current.height {
            for x in 0..current.width {
                let new_x = current.height - 1 - y;
                data[(x * width + new_x) as usize] = current.data[(y * current.width + x) as usize];
            }
        }
        current = Image3::new(width, height, data).expect("rotated dimensions");
    }
    current
}

fn pose(rng: &mut ChaCha20Rng, pattern: (u32, u32), photo: (u32, u32), fill: f64) -> Quad {
    let aspect = f64::from(pattern.0) / f64::from(pattern.1);
    let (width, height) = if aspect < 1.0 {
        let height = fill * f64::from(photo.1);
        (height * aspect, height)
    } else {
        let width = fill * f64::from(photo.0);
        (width, width / aspect)
    };
    let centre_x = f64::from(photo.0) * 0.5 + common::signed(rng, 0.02 * f64::from(photo.0));
    let centre_y = f64::from(photo.1) * 0.5 + common::signed(rng, 0.02 * f64::from(photo.1));
    let angle = common::signed(rng, 0.16);
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

fn framed(fill: f64, rng: &mut ChaCha20Rng) -> f64 {
    fill + 0.03 * common::uniform(rng)
}

fn scene(
    image: &Image3,
    quarter_turns: u8,
    photo: (u32, u32),
    background: [f32; 3],
    fill: f64,
    rng: &mut ChaCha20Rng,
) -> Image3 {
    let turned = rotate(image, quarter_turns);
    let fill = framed(fill, rng);
    let quad = pose(rng, (turned.width, turned.height), photo, fill);
    common::warp(&turned, &quad, photo.0, photo.1, background)
}

fn fine_scene(
    image: &Image3,
    quarter_turns: u8,
    photo: (u32, u32),
    background: [f32; 3],
    fill: f64,
    rng: &mut ChaCha20Rng,
) -> Image3 {
    let turned = rotate(image, quarter_turns);
    let fill = framed(fill, rng);
    let quad = pose(rng, (turned.width, turned.height), photo, fill);
    common::warp_fine(&turned, &quad, photo.0, photo.1, background)
}

fn seed(fill: u8) -> PatternSeed {
    PatternSeed::from_bytes([fill; 32])
}

fn detect(
    seed: &PatternSeed,
    scene: &Image3,
    params: &PatternParams,
) -> Result<pattern::PatternDetection, PatternError> {
    let jpeg = common::jpeg_from(scene.clone());
    pattern::detect(seed, &jpeg, scene, &common::identity_alignment(), params)
}

#[test]
fn a_clean_render_detects_every_patch_in_seed_order() {
    let seed = seed(0x21);
    let params = PatternParams::default();
    for shot in &SHOTS {
        let image = pattern::render(&seed, shot.pattern.0, shot.pattern.1).expect("renders");
        let mut rng = common::rng(0x99);
        let mut tables = Vec::new();
        for _ in 0..2 {
            let scene = scene(&image, 0, shot.photo, [0.0; 3], 0.65, &mut rng);
            let detection = detect(&seed, &scene, &params).expect("detects its own render");
            assert_eq!(detection.seed_digest, seed.digest());
            assert_eq!(detection.patches.rows().len(), PATCH_COUNT);
            for pair in detection.patches.rows().windows(2) {
                assert!(pair[0].id.get() < pair[1].id.get());
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
}

#[test]
fn another_session_seed_is_rejected() {
    let session = seed(0x31);
    let other = seed(0x32);
    let image = pattern::render(&session, PORTRAIT_PATTERN.0, PORTRAIT_PATTERN.1).expect("renders");
    let scene = scene(
        &image,
        0,
        PORTRAIT_PHOTO,
        [0.0; 3],
        0.65,
        &mut common::rng(0xab),
    );
    let result = detect(&other, &scene, &PatternParams::default());
    assert!(matches!(
        result,
        Err(PatternError::SessionMismatch { found, expected })
            if found == session.digest() && expected == other.digest()
    ));
}

#[test]
fn a_plain_scene_has_no_fiducials() {
    let scene = common::blank(PORTRAIT_PHOTO.0, PORTRAIT_PHOTO.1, [0.4, 0.45, 0.5]);
    assert!(matches!(
        detect(&seed(0x41), &scene, &PatternParams::default()),
        Err(PatternError::FiducialsNotFound)
    ));
}

#[test]
fn detection_rate_clears_the_phone_pattern_target() {
    let seed = seed(0x51);
    let params = PatternParams::default();
    let mut rng = common::rng(0x1234);
    for shot in &SHOTS {
        let image = pattern::render(&seed, shot.pattern.0, shot.pattern.1).expect("renders");
        let mut attempts = 0;
        let mut detected = 0;
        for display in &DISPLAYS {
            let image = shown(&image, display);
            for exposure in EXPOSURES {
                for _ in 0..3 {
                    attempts += 1;
                    let scene = scene(&image, 0, shot.photo, [0.01, 0.01, 0.012], 0.65, &mut rng);
                    let scene = common::scale(&scene, exposure);
                    let scene = common::blur(&scene, 2);
                    let scene = common::add_noise(&scene, 0.0015, &mut rng);
                    if detect(&seed, &scene, &params).is_ok() {
                        detected += 1;
                    }
                }
            }
        }
        assert!(
            detected * 100 >= 95 * attempts,
            "{:?}: detected {detected} of {attempts}",
            shot.pattern
        );
    }
}

#[test]
fn a_display_held_either_way_still_detects() {
    let seed = seed(0x61);
    let params = PatternParams::default();
    let mut attempts = 0;
    let mut detected = 0;
    for (pattern, upright_photo, turned_photo) in [
        (PORTRAIT_PATTERN, PORTRAIT_PHOTO, LANDSCAPE_PHOTO),
        (LANDSCAPE_PATTERN, LANDSCAPE_PHOTO, PORTRAIT_PHOTO),
    ] {
        let image = pattern::render(&seed, pattern.0, pattern.1).expect("renders");
        for display in DISPLAYS.iter().take(2) {
            let image = shown(&image, display);
            for quarter_turns in [1, 2, 3] {
                let photo = if quarter_turns % 2 == 0 {
                    upright_photo
                } else {
                    turned_photo
                };
                for _ in 0..2 {
                    attempts += 1;
                    let mut rng = common::rng(0x7700 + attempts);
                    let scene = scene(
                        &image,
                        quarter_turns,
                        photo,
                        [0.01, 0.01, 0.012],
                        0.65,
                        &mut rng,
                    );
                    let scene = common::scale(&scene, 0.5);
                    let scene = common::blur(&scene, 2);
                    let scene = common::add_noise(&scene, 0.0015, &mut rng);
                    if detect(&seed, &scene, &params).is_ok() {
                        detected += 1;
                    }
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

fn decode_png(bytes: &[u8]) -> Image3 {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
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
    Image3::new(info.width, info.height, data).expect("dimensions match the buffer")
}

#[test]
fn the_committed_fixtures_detect_with_the_published_seed() {
    let seed = fixture_seed();
    let params = PatternParams::default();
    for (bytes, size, photo) in [
        (PORTRAIT_PNG, (1080, 2340), PORTRAIT_PHOTO),
        (LANDSCAPE_PNG, (2560, 1600), LANDSCAPE_PHOTO),
    ] {
        let content = decode_png(bytes);
        assert_eq!((content.width, content.height), size);
        let layout = pattern::PatternLayout::new(content.width, content.height).expect("layout");
        assert_eq!(layout.content.x, 0.0);
        assert_eq!(layout.content.y, 0.0);
        assert_eq!(layout.content.width, f64::from(content.width));
        assert_eq!(layout.content.height, f64::from(content.height));

        for pose_index in 0..2u64 {
            let mut rng = common::rng(0x7171 + pose_index);
            let scene = fine_scene(&content, 0, photo, [0.0; 3], 0.65, &mut rng);
            let detection = detect(&seed, &scene, &params).expect("the fixture detects");
            assert_eq!(detection.seed_digest, seed.digest());
            assert_eq!(detection.patches.rows().len(), PATCH_COUNT);
        }
    }
}

/// The browser window with `content` fitted by `object-fit: contain`.
fn letterboxed(content: &Image3, window: (u32, u32)) -> Image3 {
    let content_aspect = f64::from(content.width) / f64::from(content.height);
    let window_aspect = f64::from(window.0) / f64::from(window.1);
    let (width, height) = if content_aspect > window_aspect {
        (f64::from(window.0), f64::from(window.0) / content_aspect)
    } else {
        (f64::from(window.1) * content_aspect, f64::from(window.1))
    };
    let x0 = (f64::from(window.0) - width) * 0.5;
    let y0 = (f64::from(window.1) - height) * 0.5;
    let quad = [
        Point::new(x0, y0),
        Point::new(x0 + width, y0),
        Point::new(x0 + width, y0 + height),
        Point::new(x0, y0 + height),
    ];
    common::warp_fine(content, &quad, window.0, window.1, [0.0; 3])
}

#[test]
fn a_letterboxed_browser_image_detects() {
    let seed = fixture_seed();
    let params = PatternParams::default();
    for (bytes, window, photo) in [
        (PORTRAIT_PNG, (1000, 1440), (432, 622)),
        (LANDSCAPE_PNG, (1440, 1000), (622, 432)),
    ] {
        let screen = letterboxed(&decode_png(bytes), window);
        let mut attempts = 0;
        let mut detected = 0;
        for display in &DISPLAYS {
            let screen = shown(&screen, display);
            for exposure in EXPOSURES {
                for _ in 0..2 {
                    attempts += 1;
                    let mut rng = common::rng(0x5150 + attempts);
                    let scene = fine_scene(&screen, 0, photo, [0.01, 0.01, 0.012], 0.86, &mut rng);
                    let scene = common::scale(&scene, exposure);
                    let scene = common::blur(&scene, 2);
                    let scene = common::add_noise(&scene, 0.0015, &mut rng);
                    if detect(&seed, &scene, &params).is_ok() {
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
}
