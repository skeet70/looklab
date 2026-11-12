// SPDX-License-Identifier: AGPL-3.0-or-later

use jpeg_encoder::{ColorType, Encoder};
use looklab_color::spaces::RgbSpace;
use looklab_prep::{
    DecodedJpeg, Image3, LocalImageKey, PREP_VERSION, PrepParams, PriorTransform, SamplingSeed,
    SeedFile, prepare_decoded, prepare_pair, prepare_pair_decoded,
};
use looklab_raw::{DecodedRaw, Orientation, RawDecoder, RawlerDecoder};
use looklab_samples::{
    MAX_DENSITY_WEIGHT, MAX_RGB, MIN_RGB, OutputSpace, PrepVersion, PriorTransformVersion, encode,
};

const WIDTH: u32 = 96;
const HEIGHT: u32 = 72;

fn scene() -> Image3 {
    let mut data = Vec::with_capacity((WIDTH * HEIGHT) as usize);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let u = x as f32 / (WIDTH - 1) as f32;
            let v = y as f32 / (HEIGHT - 1) as f32;
            let field = 0.08 + 0.75 * (u * (1.0 - v) + 0.12);
            data.push([
                field * (1.0 - 0.35 * v),
                field * (0.55 + 0.35 * u),
                field * (0.35 + 0.5 * (1.0 - u)),
            ]);
        }
    }
    Image3::new(WIDTH, HEIGHT, data).expect("dimensions match")
}

fn scaled(image: &Image3, factor: f32) -> Image3 {
    Image3::new(
        image.width,
        image.height,
        image
            .data
            .iter()
            .map(|rgb| [rgb[0] * factor, rgb[1] * factor, rgb[2] * factor])
            .collect(),
    )
    .expect("dimensions match")
}

fn raw_from(image: &Image3, wb: [f64; 3]) -> DecodedRaw {
    let rgb = image
        .data
        .iter()
        .flat_map(|px| {
            [
                (f64::from(px[0]) / wb[0]) as f32,
                (f64::from(px[1]) / wb[1]) as f32,
                (f64::from(px[2]) / wb[2]) as f32,
            ]
        })
        .collect();
    DecodedRaw {
        width: image.width as usize,
        height: image.height as usize,
        rgb,
        as_shot_wb: wb,
        orientation: Orientation::Normal,
        make: "FUJIFILM".to_owned(),
        model: "X-S10".to_owned(),
        color_matrices: None,
    }
}

fn jpeg_from(image: Image3) -> DecodedJpeg {
    let code_values_near_clip = image
        .data
        .iter()
        .map(|rgb| rgb.iter().any(|channel| *channel >= 0.98))
        .collect();
    DecodedJpeg {
        image,
        space: OutputSpace::Srgb,
        space_assumed: false,
        code_values_near_clip,
    }
}

fn fixed_seed(fill: u8) -> SamplingSeed {
    let hex: String = format!("{fill:02x}").repeat(32);
    let json = format!(
        r#"{{"format":"looklab-seeds/1","prior_transform_version":1,"entries":[{{"key":"A.RAF","seed":"{hex}"}}]}}"#
    );
    SeedFile::from_json(&json)
        .expect("valid seed file")
        .seed_for(&LocalImageKey::new("A.RAF").expect("valid key"))
        .expect("entry exists")
}

fn prior() -> PriorTransform {
    PriorTransform::new(
        RgbSpace::Srgb.to_xyz_matrix(),
        5500.0,
        PriorTransformVersion::new(3),
    )
}

fn params() -> PrepParams {
    PrepParams::default()
}

fn all_samples_in_limits(samples: &looklab_samples::SampleSet) {
    for rgb in samples.in_rgb().iter().chain(samples.out_rgb()) {
        for channel in rgb {
            assert!(channel.is_finite());
            assert!((MIN_RGB..=MAX_RGB).contains(channel));
        }
    }
    for weight in samples.density_w() {
        assert!(weight.is_finite() && *weight >= 1.0 && *weight <= MAX_DENSITY_WEIGHT);
    }
}

fn cropped_scene() -> Image3 {
    let scene = scene();
    let mut data = Vec::with_capacity((WIDTH * 54) as usize);
    for y in 9..63 {
        for x in 0..WIDTH {
            data.push(scene.data[(y * WIDTH + x) as usize]);
        }
    }
    Image3::new(WIDTH, 54, data).expect("dimensions match")
}

fn narrow_cropped_scene() -> Image3 {
    let scene = scene();
    let mut data = Vec::with_capacity((WIDTH * 20) as usize);
    for y in 26..46 {
        for x in 0..WIDTH {
            data.push(scene.data[(y * WIDTH + x) as usize]);
        }
    }
    Image3::new(WIDTH, 20, data).expect("dimensions match")
}

#[test]
fn prepares_a_sixteen_by_nine_crop() {
    let scene = scene();
    let raw = raw_from(&scene, [2.0, 1.0, 1.5]);
    let jpeg = jpeg_from(cropped_scene());
    let samples = prepare_pair_decoded(&raw, &jpeg, &prior(), fixed_seed(0x21), &params())
        .expect("a centred 16:9 crop prepares");
    assert!(samples.n() > 0);
    all_samples_in_limits(&samples);
}

#[test]
fn prepares_a_cropped_two_member_group() {
    let scene = scene();
    let raw = raw_from(&scene, [2.0, 1.0, 1.5]);
    let first = jpeg_from(cropped_scene());
    let second = jpeg_from(narrow_cropped_scene());
    let sets = prepare_decoded(
        &raw,
        &[&first, &second],
        &prior(),
        fixed_seed(0x22),
        &params(),
    )
    .expect("a cropped two-member group prepares");
    assert_eq!(sets.len(), 2);
    assert_eq!(sets[0].in_rgb(), sets[1].in_rgb());
    for set in &sets {
        assert!(set.n() > 0);
        all_samples_in_limits(set);
    }
}

#[test]
fn prepares_a_pair_with_identity_output() {
    let scene = scene();
    let raw = raw_from(&scene, [2.0, 1.0, 1.5]);
    let jpeg = jpeg_from(scene.clone());
    let samples =
        prepare_pair_decoded(&raw, &jpeg, &prior(), fixed_seed(0x5a), &params()).expect("prepares");

    assert!(samples.n() > 10);
    assert_eq!(samples.header().n as usize, samples.n());
    assert_eq!(
        samples.header().prep_version,
        PrepVersion::new(PREP_VERSION)
    );
    assert_eq!(samples.header().output_space, OutputSpace::Srgb);
    assert_eq!(
        samples.header().prior_transform_version,
        PriorTransformVersion::new(3)
    );
    assert_eq!(samples.header().wb_multipliers, [2.0, 1.0, 1.5]);
    assert!(samples.header().rerender_group.is_none());
    assert_eq!(
        samples.header().prep_params_hash,
        params().hash().expect("hash")
    );
    all_samples_in_limits(&samples);

    for (in_rgb, out_rgb) in samples.in_rgb().iter().zip(samples.out_rgb()) {
        for (a, b) in in_rgb.iter().zip(out_rgb) {
            assert!((a - b).abs() < 0.05, "{in_rgb:?} vs {out_rgb:?}");
        }
    }
}

#[test]
fn group_members_share_input_pixels_and_shuffle() {
    let scene = scene();
    let raw = raw_from(&scene, [2.0, 1.0, 1.5]);
    let brighter = jpeg_from(scene.clone());
    let darker = jpeg_from(scaled(&scene, 0.7));
    let sets = prepare_decoded(
        &raw,
        &[&brighter, &darker],
        &prior(),
        fixed_seed(0x33),
        &params(),
    )
    .expect("prepares");
    assert_eq!(sets.len(), 2);

    assert_eq!(sets[0].in_rgb(), sets[1].in_rgb());
    assert_eq!(sets[0].density_w(), sets[1].density_w());
    assert_eq!(
        sets[0].header().rerender_group,
        sets[1].header().rerender_group
    );
    assert!(sets[0].header().rerender_group.is_some());
    assert_ne!(sets[0].out_rgb(), sets[1].out_rgb());
}

#[test]
fn a_group_member_that_does_not_align_with_the_raw_is_rejected() {
    let scene = scene();
    let raw = raw_from(&scene, [2.0, 1.0, 1.5]);
    let reference = jpeg_from(scene);
    let flat = Image3::new(
        WIDTH,
        HEIGHT,
        vec![[0.3, 0.3, 0.3]; (WIDTH * HEIGHT) as usize],
    )
    .expect("dimensions match");
    let error = prepare_decoded(
        &raw,
        &[&reference, &jpeg_from(flat)],
        &prior(),
        fixed_seed(0x33),
        &params(),
    )
    .expect_err("a render with no structure cannot be aligned");
    assert!(matches!(
        error,
        looklab_prep::PrepError::MemberAlignment { index: 2, .. }
    ));
}

#[test]
fn same_seed_reproduces_blobs_and_a_different_seed_reorders() {
    let scene = scene();
    let raw = raw_from(&scene, [2.0, 1.0, 1.5]);
    let jpeg = jpeg_from(scene);
    let run =
        |seed| prepare_pair_decoded(&raw, &jpeg, &prior(), seed, &params()).expect("prepares");
    let first = run(fixed_seed(0x01));
    let repeat = run(fixed_seed(0x01));
    assert_eq!(
        encode(&first).expect("encodes"),
        encode(&repeat).expect("encodes")
    );

    let other = run(fixed_seed(0x02));
    assert_eq!(other.n(), first.n());
    assert_ne!(
        encode(&first).expect("encodes"),
        encode(&other).expect("encodes")
    );
    assert_ne!(first.in_rgb(), other.in_rgb());
}

#[test]
fn non_finite_raw_pixels_never_reach_the_samples() {
    let scene = scene();
    let mut raw = raw_from(&scene, [2.0, 1.0, 1.5]);
    for y in 20..24 {
        for x in 30..34 {
            let base = (y * scene.width as usize + x) * 3;
            raw.rgb[base] = f32::NAN;
        }
    }
    let jpeg = jpeg_from(scene);
    let samples =
        prepare_pair_decoded(&raw, &jpeg, &prior(), fixed_seed(0x04), &params()).expect("prepares");
    assert!(samples.n() > 0);
    all_samples_in_limits(&samples);
}

#[test]
fn a_non_finite_jpeg_sample_is_an_error() {
    let scene = scene();
    let raw = raw_from(&scene, [2.0, 1.0, 1.5]);
    let mut data = scene.data.clone();
    for pixel in &mut data {
        pixel[0] = f32::NAN;
    }
    let jpeg = jpeg_from(Image3::new(WIDTH, HEIGHT, data).expect("dimensions match"));
    let result = prepare_pair_decoded(&raw, &jpeg, &prior(), fixed_seed(0x05), &params());
    assert!(result.is_err());
}

#[test]
fn random_pixels_never_store_a_non_finite_sample() {
    use rand_chacha::ChaCha20Rng;
    use rand_chacha::rand_core::{RngCore, SeedableRng};

    let mut rng = ChaCha20Rng::from_seed([0x5a; 32]);
    for _ in 0..8 {
        let width = 24 + rng.next_u32() % 17;
        let height = 24 + rng.next_u32() % 17;
        let pixels = |rng: &mut ChaCha20Rng| -> Vec<[f32; 3]> {
            (0..(width * height))
                .map(|_| {
                    [
                        f32::from_bits(rng.next_u32()),
                        f32::from_bits(rng.next_u32()),
                        f32::from_bits(rng.next_u32()),
                    ]
                })
                .collect()
        };
        let raw = DecodedRaw {
            width: width as usize,
            height: height as usize,
            rgb: pixels(&mut rng).into_iter().flatten().collect(),
            as_shot_wb: [2.0, 1.0, 1.5],
            orientation: Orientation::Normal,
            make: "FUJIFILM".to_owned(),
            model: "X-S10".to_owned(),
            color_matrices: None,
        };
        let jpegs: Vec<DecodedJpeg> = (0..2)
            .map(|_| {
                jpeg_from(Image3::new(width, height, pixels(&mut rng)).expect("dimensions match"))
            })
            .collect();
        let members: Vec<&DecodedJpeg> = jpegs.iter().collect();
        if let Ok(sets) = prepare_decoded(&raw, &members, &prior(), fixed_seed(0x06), &params()) {
            for set in &sets {
                all_samples_in_limits(set);
            }
        }
    }

    let mut data = scene().data;
    for _ in 0..(data.len() / 20) {
        let index = (rng.next_u32() as usize) % data.len();
        let channel = (rng.next_u32() % 3) as usize;
        data[index][channel] = f32::from_bits(rng.next_u32());
    }
    let raw = DecodedRaw {
        width: WIDTH as usize,
        height: HEIGHT as usize,
        rgb: data.iter().flatten().copied().collect(),
        as_shot_wb: [2.0, 1.0, 1.5],
        orientation: Orientation::Normal,
        make: "FUJIFILM".to_owned(),
        model: "X-S10".to_owned(),
        color_matrices: None,
    };
    let jpeg = jpeg_from(scene());
    match prepare_pair_decoded(&raw, &jpeg, &prior(), fixed_seed(0x07), &params()) {
        Ok(samples) => {
            assert!(samples.n() > 0);
            all_samples_in_limits(&samples);
        }
        Err(looklab_prep::PrepError::AlignLowCorrelation { .. })
        | Err(looklab_prep::PrepError::AlignNoOverlap) => {}
        Err(error) => panic!("unexpected error: {error}"),
    }
}

#[test]
fn the_params_hash_covers_the_params() {
    let a = params();
    let b = params();
    assert_eq!(a.hash().expect("hash"), b.hash().expect("hash"));
    let mut changed = params();
    changed.downsample_max_pixels += 1;
    assert_ne!(a.hash().expect("hash"), changed.hash().expect("hash"));
}

#[allow(clippy::disallowed_methods)]
fn x_s10_raf() -> Vec<u8> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/cache/fujifilm-x-s10.raf");
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "read {}: {error}; run tools/fetch-fixtures.sh",
            path.display()
        )
    })
}

fn jpeg_bytes_from(image: &Image3, quality: u8) -> Vec<u8> {
    let mut bytes = Vec::new();
    let encoded: Vec<u8> = image
        .data
        .iter()
        .flat_map(|rgb| {
            rgb.map(|channel| {
                let linear = f64::from(channel.clamp(0.0, 1.0));
                (RgbSpace::Srgb.encode(linear) * 255.0).round() as u8
            })
        })
        .collect();
    let encoder = Encoder::new(&mut bytes, quality);
    encoder
        .encode(
            &encoded,
            image.width as u16,
            image.height as u16,
            ColorType::Rgb,
        )
        .expect("encodes jpeg");
    bytes
}

fn meta_orientation(orientation: Orientation) -> looklab_meta::Orientation {
    match orientation {
        Orientation::Normal => looklab_meta::Orientation::Normal,
        Orientation::HorizontalFlip => looklab_meta::Orientation::FlipHorizontal,
        Orientation::Rotate180 => looklab_meta::Orientation::Rotate180,
        Orientation::VerticalFlip => looklab_meta::Orientation::FlipVertical,
        Orientation::Transpose => looklab_meta::Orientation::Transpose,
        Orientation::Rotate90 => looklab_meta::Orientation::Rotate90Clockwise,
        Orientation::Transverse => looklab_meta::Orientation::Transverse,
        Orientation::Rotate270 => looklab_meta::Orientation::Rotate270Clockwise,
    }
}

fn inverse_orientation(orientation: Orientation) -> Orientation {
    match orientation {
        Orientation::Rotate90 => Orientation::Rotate270,
        Orientation::Rotate270 => Orientation::Rotate90,
        other => other,
    }
}

#[test]
fn the_dng_prior_takes_white_balanced_input() {
    let decoded = RawlerDecoder.decode(&x_s10_raf()).expect("fixture decodes");
    let matrices = decoded
        .color_matrices
        .as_ref()
        .expect("the fixture reports colour matrices");
    let prior = PriorTransform::from_camera_matrices(
        matrices,
        decoded.as_shot_wb,
        PriorTransformVersion::new(1),
    )
    .expect("prior transform");
    let white = prior.matrix().mul_vec([1.0, 1.0, 1.0]);
    let sum = white[0] + white[1] + white[2];
    let (x, y) = (white[0] / sum, white[1] / sum);
    assert!((x - 0.3127).abs() < 0.01, "white x {x}");
    assert!((y - 0.3290).abs() < 0.01, "white y {y}");
}

fn flat_raw(pixel: impl Fn(usize) -> [f32; 3], wb: [f64; 3]) -> DecodedRaw {
    let mut rgb = Vec::with_capacity((WIDTH * HEIGHT * 3) as usize);
    for index in 0..(WIDTH * HEIGHT) as usize {
        rgb.extend_from_slice(&pixel(index));
    }
    DecodedRaw {
        width: WIDTH as usize,
        height: HEIGHT as usize,
        rgb,
        as_shot_wb: wb,
        orientation: Orientation::Normal,
        make: "FUJIFILM".to_owned(),
        model: "X-S10".to_owned(),
        color_matrices: None,
    }
}

fn wb_image(raw: &DecodedRaw) -> Image3 {
    let (pixels, _) = raw.rgb.as_chunks::<3>();
    let data = pixels
        .iter()
        .map(|px| {
            [
                (f64::from(px[0]) * raw.as_shot_wb[0]) as f32,
                (f64::from(px[1]) * raw.as_shot_wb[1]) as f32,
                (f64::from(px[2]) * raw.as_shot_wb[2]) as f32,
            ]
        })
        .collect();
    Image3::new(raw.width as u32, raw.height as u32, data).expect("dimensions match")
}

#[test]
fn raw_clipping_is_judged_before_white_balance() {
    let target = 40 * WIDTH as usize + 40;
    let raw = flat_raw(
        |index| {
            if index == target {
                [0.6, 0.05, 0.05]
            } else {
                [0.1, 0.1, 0.1]
            }
        },
        [2.0, 1.0, 1.0],
    );
    let jpeg = jpeg_from(wb_image(&raw));
    let samples =
        prepare_pair_decoded(&raw, &jpeg, &prior(), fixed_seed(0x21), &params()).expect("prepares");
    assert!(
        samples
            .in_rgb()
            .iter()
            .any(|rgb| rgb[0] > 1.1 && rgb[1] < 0.1)
    );

    let target = 40 * WIDTH as usize + 40;
    let raw = flat_raw(
        |index| {
            if index == target {
                [0.99, 0.01, 0.01]
            } else {
                [0.2, 0.2, 0.2]
            }
        },
        [0.5, 1.0, 1.0],
    );
    let jpeg = jpeg_from(wb_image(&raw));
    let samples =
        prepare_pair_decoded(&raw, &jpeg, &prior(), fixed_seed(0x22), &params()).expect("prepares");
    assert!(
        !samples
            .in_rgb()
            .iter()
            .any(|rgb| { (0.47..0.52).contains(&rgb[0]) && (0.005..0.015).contains(&rgb[1]) })
    );
}

#[test]
fn samples_average_the_jpeg_over_the_raw_footprint() {
    let scene = scene();
    let raw = raw_from(&scene, [1.0, 1.0, 1.0]);
    let jpeg = jpeg_from(scene);
    let params = PrepParams {
        downsample_max_pixels: 2_000,
        ..params()
    };
    let samples =
        prepare_pair_decoded(&raw, &jpeg, &prior(), fixed_seed(0x23), &params).expect("prepares");
    assert!(samples.n() > 10);
    for (in_rgb, out_rgb) in samples.in_rgb().iter().zip(samples.out_rgb()) {
        for (a, b) in in_rgb.iter().zip(out_rgb) {
            assert!((a - b).abs() < 0.08, "{in_rgb:?} vs {out_rgb:?}");
        }
    }
}

/// A ramp constant within each `factor` by `factor` block.
fn block_ramp_raw(width: u32, height: u32, factor: u32) -> DecodedRaw {
    let down_width = width / factor;
    let down_height = height / factor;
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            rgb.extend_from_slice(&[
                0.05 + 0.9 * (x / factor) as f32 / (down_width - 1) as f32,
                0.05 + 0.9 * (y / factor) as f32 / (down_height - 1) as f32,
                0.5,
            ]);
        }
    }
    DecodedRaw {
        width: width as usize,
        height: height as usize,
        rgb,
        as_shot_wb: [1.0, 1.0, 1.0],
        orientation: Orientation::Normal,
        make: "FUJIFILM".to_owned(),
        model: "X-S10".to_owned(),
        color_matrices: None,
    }
}

fn down_ramp_jpeg(width: u32, height: u32) -> DecodedJpeg {
    let mut data = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            data.push([
                0.05 + 0.9 * x as f32 / (width - 1) as f32,
                0.05 + 0.9 * y as f32 / (height - 1) as f32,
                0.5,
            ]);
        }
    }
    jpeg_from(Image3::new(width, height, data).expect("dimensions match"))
}

#[test]
fn secondary_members_sample_in_the_downsampled_frame() {
    const WIDTH: u32 = 320;
    const HEIGHT: u32 = 240;
    const FACTOR: u32 = 8;
    const DOWN_WIDTH: u32 = WIDTH / FACTOR;
    const DOWN_HEIGHT: u32 = HEIGHT / FACTOR;

    let raw = block_ramp_raw(WIDTH, HEIGHT, FACTOR);
    let reference = jpeg_from(wb_image(&raw));
    let secondary = down_ramp_jpeg(DOWN_WIDTH, DOWN_HEIGHT);
    let params = PrepParams {
        downsample_max_pixels: DOWN_WIDTH * DOWN_HEIGHT,
        ..params()
    };
    let sets = prepare_decoded(
        &raw,
        &[&reference, &secondary],
        &prior(),
        fixed_seed(0x40),
        &params,
    )
    .expect("prepares");
    assert_eq!(sets.len(), 2);

    // A secondary fit is in the downsampled frame; its samples land on the
    // down-ramp lattice.
    let step = 0.9 / f64::from(DOWN_WIDTH - 1);
    for (index, rgb) in sets[1].out_rgb().iter().enumerate() {
        let lattice = (f64::from(rgb[0]) - 0.05) / step;
        let error = (lattice - lattice.round()).abs();
        assert!(
            error < 0.1,
            "sample {index} channel 0 = {} is {error} off the downsampled ramp lattice",
            rgb[0]
        );
    }
}

#[test]
fn every_raw_orientation_samples_as_the_oriented_image() {
    let mut base = scene();
    for y in 8..12 {
        for x in 16..20 {
            base.data[y * base.width as usize + x] = [2.0, 1.0, 1.5];
        }
    }
    let params = PrepParams {
        downsample_max_pixels: 2_000,
        ..params()
    };
    let prepare = |orientation: Orientation| {
        let sensor = base.apply_orientation(meta_orientation(inverse_orientation(orientation)));
        let mut raw = raw_from(&sensor, [2.0, 1.0, 1.5]);
        raw.orientation = orientation;
        let jpeg = jpeg_from(base.clone());
        encode(
            &prepare_pair_decoded(&raw, &jpeg, &prior(), fixed_seed(0x30), &params)
                .expect("prepares"),
        )
        .expect("encodes")
    };
    let reference = prepare(Orientation::Normal);
    for orientation in [
        Orientation::Normal,
        Orientation::HorizontalFlip,
        Orientation::Rotate180,
        Orientation::VerticalFlip,
        Orientation::Transpose,
        Orientation::Rotate90,
        Orientation::Transverse,
        Orientation::Rotate270,
    ] {
        assert_eq!(prepare(orientation), reference, "{orientation:?}");
    }
}

#[test]
fn prepares_the_x_s10_fixture_with_a_rendered_jpeg() {
    let raw_bytes = x_s10_raf();
    let decoded = RawlerDecoder.decode(&raw_bytes).expect("fixture decodes");
    let wb = decoded.as_shot_wb;
    let (pixels, _) = decoded.rgb.as_chunks::<3>();
    let data: Vec<[f32; 3]> = pixels
        .iter()
        .map(|px| {
            [
                (f64::from(px[0]) * wb[0]) as f32,
                (f64::from(px[1]) * wb[1]) as f32,
                (f64::from(px[2]) * wb[2]) as f32,
            ]
        })
        .collect();
    let image = Image3::new(decoded.width as u32, decoded.height as u32, data)
        .expect("dimensions match")
        .apply_orientation(meta_orientation(decoded.orientation));
    let jpeg_bytes = jpeg_bytes_from(&image, 95);
    let matrices = decoded
        .color_matrices
        .as_ref()
        .expect("the fixture reports colour matrices");
    let dng_prior =
        PriorTransform::from_camera_matrices(matrices, wb, PriorTransformVersion::new(3))
            .expect("prior transform");
    assert!((2000.0..=10_000.0).contains(&dng_prior.cct_k()));

    let prepared = prepare_pair(
        &raw_bytes,
        &jpeg_bytes,
        &dng_prior,
        fixed_seed(0x10),
        &params(),
    )
    .expect("prepares the fixture");

    assert!(prepared.samples.n() > 100);
    assert!(prepared.samples.n() <= looklab_samples::MAX_SAMPLES as usize);
    assert_eq!(prepared.sanitized.make.as_deref(), Some("FUJIFILM"));
    assert!(prepared.sanitized.as_shot_wb.is_some());
    all_samples_in_limits(&prepared.samples);
}
