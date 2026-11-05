// SPDX-License-Identifier: AGPL-3.0-or-later
//! A `LookFunction` backed by a published document's probe covariance and density.

use std::collections::HashMap;

use looklab_color::Xyz;
use looklab_color::ucs::Ucs;
use looklab_model::{LayerValues, Look, LookDocument, LookInput, Probe, ProbeId, ProbeSet};
use looklab_report::compare::{Covariance3, LookFunction, Support};

/// `σ_ΔE` above which a probe is unsupported (docs/02 §5).
pub const SUPPORT_SIGMA: f64 = 1.5;

pub struct DocumentLook<'a> {
    document: &'a LookDocument,
    index: HashMap<ProbeId, usize>,
}

impl<'a> DocumentLook<'a> {
    pub fn new(document: &'a LookDocument, probes: &ProbeSet) -> Self {
        let index = probes
            .probes
            .iter()
            .enumerate()
            .map(|(position, probe)| (probe.id.clone(), position))
            .collect();
        Self { document, index }
    }

    fn probe_position(&self, probe: &Probe) -> Option<usize> {
        self.index.get(&probe.id).copied()
    }
}

impl LookFunction for DocumentLook<'_> {
    fn predict(&self, xyz: Xyz) -> Ucs {
        self.document.look.eval(xyz)
    }

    fn covariance(&self, probe: &Probe) -> Option<Covariance3> {
        let position = self.probe_position(probe)?;
        let uncertainty = &self.document.uncertainty;
        let covariance = uncertainty
            .probe_cov_post
            .as_ref()
            .or(uncertainty.probe_cov.as_ref())?;
        let values = covariance.values.get(position * 9..position * 9 + 9)?;
        Some(Covariance3 {
            jj: values[0],
            ja: values[1],
            jb: values[2],
            aa: values[4],
            ab: values[5],
            bb: values[8],
        })
    }

    fn support(&self, probe: &Probe) -> Support {
        let Some(position) = self.probe_position(probe) else {
            return Support::Supported;
        };
        if let Some(mask) = self
            .document
            .uncertainty
            .support
            .as_ref()
            .and_then(|mask| mask.values.get(position))
        {
            return if *mask > 0.5 {
                Support::Supported
            } else {
                Support::Unsupported
            };
        }
        let density = self
            .document
            .uncertainty
            .density
            .as_ref()
            .and_then(|density| density.values.get(position))
            .copied()
            .unwrap_or(f64::INFINITY);
        let sigma = self.covariance(probe).map_or(0.0, Covariance3::sigma);
        if sigma <= SUPPORT_SIGMA && density > 0.0 {
            Support::Supported
        } else {
            Support::Unsupported
        }
    }

    fn layer_values(&self, input: &LookInput) -> Option<LayerValues> {
        Some(self.document.look.layer_values(input))
    }

    fn look(&self) -> Option<&Look> {
        Some(&self.document.look)
    }
}

pub struct DifferenceImage {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub async fn difference_image(
    a: &[u8],
    b: &[u8],
    scene: &looklab_publish::PreparedScene,
) -> Result<DifferenceImage, String> {
    use looklab_color::cam16::{Cam16Model, Jmh, ViewingConditions};
    use looklab_color::ucs::{UcsSpace, delta_e_ucs, jmh_to_ucs};
    use looklab_color::{Rgb, RgbSpace};

    let a = looklab_publish::render_preview(a, scene).map_err(|error| error.to_string())?;
    let b = looklab_publish::render_preview(b, scene).map_err(|error| error.to_string())?;
    let step = scene
        .width()
        .div_ceil(400)
        .max(scene.height().div_ceil(300))
        .max(1);
    let width = scene.width().div_ceil(step);
    let height = scene.height().div_ceil(step);
    let model = Cam16Model::new(&ViewingConditions::reference());
    let to_ucs = |pixel: &[u8]| {
        let xyz = RgbSpace::Srgb.rgb_encoded_to_xyz(Rgb::new(
            f64::from(pixel[0]) / 255.0,
            f64::from(pixel[1]) / 255.0,
            f64::from(pixel[2]) / 255.0,
        ));
        let cam = model.xyz_to_cam16(xyz);
        jmh_to_ucs(
            Jmh {
                j: cam.j,
                m: cam.m,
                h: cam.h,
            },
            UcsSpace::Ucs,
        )
    };
    // Cividis sample anchors, with a fixed 0..10 CAM16-UCS display range.
    let palette = [
        [0., 32., 77.],
        [67., 78., 108.],
        [124., 123., 120.],
        [188., 173., 108.],
        [255., 233., 69.],
    ];
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let index = (((y * step) * scene.width() + x * step) * 4) as usize;
            let delta = delta_e_ucs(to_ucs(&a[index..index + 3]), to_ucs(&b[index..index + 3]));
            let position = (delta / 10.0).clamp(0.0, 1.0) * 4.0;
            let low = (position.floor() as usize).min(3);
            let fraction = position - low as f64;
            for (start, end) in palette[low].iter().zip(palette[low + 1].iter()) {
                rgba.push((start * (1.0 - fraction) + end * fraction).round() as u8);
            }
            rgba.push(255);
        }
        if y % 4 == 0 {
            let _ = wasm_bindgen_futures::JsFuture::from(crate::ui::yield_browser()).await;
        }
    }
    Ok(DifferenceImage {
        rgba,
        width,
        height,
    })
}
