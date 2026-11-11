// SPDX-License-Identifier: AGPL-3.0-or-later

use jpeg_encoder::{ColorType, Encoder};
use looklab_color::RgbSpace;
use looklab_prep::{Image3, PrepParams};
use looklab_prep_module::{
    Function, ModuleError, PrepareRequest, PreparedImageView, PriorTransformSpec, Response,
    dispatch,
};
use looklab_raw::{DecodedRaw, Orientation, RawDecoder, RawlerDecoder};
use looklab_samples::{InputSpace, OutputSpace, SampleSet};

fn dng() -> Vec<u8> {
    include_bytes!("../../../fixtures/canon-eos-60d.dng").to_vec()
}

#[allow(clippy::disallowed_methods)]
fn x_s10() -> Vec<u8> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/cache/fujifilm-x-s10.raf");
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "read {}: {error}; run tools/fetch-fixtures.sh",
            path.display()
        )
    })
}

fn raw_image(decoded: &DecodedRaw) -> Image3 {
    let wb = decoded.as_shot_wb;
    let (pixels, _) = decoded.rgb.as_chunks::<3>();
    let data = pixels
        .iter()
        .map(|pixel| {
            [
                (f64::from(pixel[0]) * wb[0]) as f32,
                (f64::from(pixel[1]) * wb[1]) as f32,
                (f64::from(pixel[2]) * wb[2]) as f32,
            ]
        })
        .collect();
    Image3::new(decoded.width as u32, decoded.height as u32, data)
        .expect("dimensions match")
        .apply_orientation(orientation(decoded.orientation))
}

fn orientation(orientation: Orientation) -> looklab_meta::Orientation {
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

fn jpeg(image: &Image3) -> Vec<u8> {
    let codes: Vec<u8> = image
        .data
        .iter()
        .flat_map(|rgb| {
            rgb.map(|channel| {
                let linear = f64::from(channel.clamp(0.0, 1.0));
                (RgbSpace::Srgb.encode(linear) * 255.0).round() as u8
            })
        })
        .collect();
    let mut bytes = Vec::new();
    Encoder::new(&mut bytes, 95)
        .encode(
            &codes,
            image.width as u16,
            image.height as u16,
            ColorType::Rgb,
        )
        .expect("JPEG encodes");
    bytes
}

fn grey(image: &Image3) -> Image3 {
    let data = image
        .data
        .iter()
        .map(|rgb| {
            let luma = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
            [luma, luma, luma]
        })
        .collect();
    Image3::new(image.width, image.height, data).expect("dimensions match")
}

fn sepia(image: &Image3) -> Image3 {
    let data = image
        .data
        .iter()
        .map(|rgb| {
            let luma = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
            [luma * 1.07, luma * 0.74, luma * 0.43]
        })
        .collect();
    Image3::new(image.width, image.height, data).expect("dimensions match")
}

fn prepare_views(raw: Vec<u8>, jpegs: Vec<Vec<u8>>, params: PrepParams) -> Vec<PreparedImageView> {
    let request = PrepareRequest {
        raw,
        jpegs,
        prior: PriorTransformSpec::CameraMatrices { version: 3 },
        seed: [0x2a; 32],
        params,
    };
    let input = postcard::to_allocvec(&request).expect("request encodes");
    let output = dispatch(Function::Prepare.id(), &input);
    let result: Result<Response, ModuleError> =
        postcard::from_bytes(&output).expect("reply decodes");
    match result.expect("the mixed group prepares") {
        Response::Prepare(response) => response.images,
        other => panic!("unexpected response {other:?}"),
    }
}

fn prepare(raw: Vec<u8>, jpegs: Vec<Vec<u8>>) -> Vec<SampleSet> {
    let images = prepare_views(raw, jpegs, PrepParams::default());
    assert_eq!(images.len(), 3);
    images
        .iter()
        .map(|image| {
            assert!(image.alignment.correlation > 0.98);
            looklab_samples::decode(&image.blob).expect("sample blob decodes")
        })
        .collect()
}

/// Shifts each row horizontally by `factor` times its offset from the centre.
fn shear(image: &Image3, factor: f64) -> Image3 {
    let width = image.width as usize;
    let height = image.height as usize;
    let centre = (height as f64 - 1.0) / 2.0;
    let mut data = Vec::with_capacity(width * height);
    for y in 0..height {
        let shift = (factor * (y as f64 - centre)).round() as i64;
        for x in 0..width {
            let source = (x as i64 + shift).clamp(0, width as i64 - 1) as usize;
            data.push(image.data[y * width + source]);
        }
    }
    Image3::new(image.width, image.height, data).expect("dimensions match")
}

fn synthetic() -> Vec<SampleSet> {
    let raw = dng();
    let decoded = RawlerDecoder.decode(&raw).expect("fixture decodes");
    let base = raw_image(&decoded);
    prepare(
        raw,
        vec![jpeg(&base), jpeg(&grey(&base)), jpeg(&sepia(&base))],
    )
}

fn bits(set: &SampleSet) -> Vec<[u32; 3]> {
    set.in_rgb()
        .iter()
        .map(|rgb| rgb.map(f32::to_bits))
        .collect()
}

fn channel_means(set: &SampleSet) -> [f64; 3] {
    let mut sums = [0.0f64; 3];
    for rgb in set.out_rgb() {
        for channel in 0..3 {
            sums[channel] += f64::from(rgb[channel]);
        }
    }
    let n = set.n() as f64;
    sums.map(|sum| sum / n)
}

fn assert_shared(sets: &[SampleSet]) {
    let reference = &sets[0];
    for set in sets {
        assert_eq!(bits(set), bits(reference));
        assert_eq!(set.density_w(), reference.density_w());
        assert_eq!(
            set.header().rerender_group,
            reference.header().rerender_group
        );
        assert!(set.header().rerender_group.is_some());
        assert_eq!(set.header().input_space, InputSpace::CameraRgbWbLinear);
        assert_eq!(set.header().output_space, OutputSpace::Srgb);
    }
}

fn assert_toned(sets: &[SampleSet]) {
    let grey = channel_means(&sets[1]);
    let sepia = channel_means(&sets[2]);

    assert!((grey[0] - grey[1]).abs() < 2e-3, "{grey:?}");
    assert!((grey[1] - grey[2]).abs() < 2e-3, "{grey:?}");

    assert!((sepia[0] / sepia[1] - 1.45).abs() < 0.05, "{sepia:?}");
    assert!((sepia[2] / sepia[1] - 0.58).abs() < 0.05, "{sepia:?}");

    assert!(
        sets[0].out_rgb().iter().any(|rgb| {
            let max = rgb.iter().copied().fold(f32::MIN, f32::max);
            let min = rgb.iter().copied().fold(f32::MAX, f32::min);
            max - min > 0.02
        }),
        "the colour render keeps chroma"
    );
}

#[test]
fn a_mixed_group_shares_its_input_pixels_and_selection() {
    assert_shared(&synthetic());
}

#[test]
fn a_mixed_group_preserves_each_renders_toned_channels() {
    assert_toned(&synthetic());
}

#[test]
fn each_render_reports_its_own_quality_in_original_raw_pixels() {
    let raw = x_s10();
    let decoded = RawlerDecoder.decode(&raw).expect("fixture decodes");
    let base = raw_image(&decoded);
    let grey = grey(&base);
    let slight = jpeg(&shear(&grey, 0.01));
    let warped = jpeg(&shear(&grey, 0.05));
    let mut params = PrepParams::default();
    params.align.min_central_correlation = 0.3;
    params.downsample_max_pixels = 50_000;

    let images = prepare_views(
        raw,
        vec![slight.clone(), slight, jpeg(&grey), warped],
        params,
    );
    assert_eq!(images.len(), 4);
    assert!(
        images[0].alignment.residual_px > 0.1,
        "full-RAW residual {}",
        images[0].alignment.residual_px
    );
    assert!(
        images[1].alignment.residual_px > 0.5 * images[0].alignment.residual_px,
        "secondary residual {} vs full-RAW {}",
        images[1].alignment.residual_px,
        images[0].alignment.residual_px
    );
    assert!(
        images[2].alignment.correlation > images[3].alignment.correlation + 1e-3,
        "sharp {:?} vs warped {:?}",
        images[2].alignment,
        images[3].alignment
    );
}
