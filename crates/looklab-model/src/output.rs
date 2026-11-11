// SPDX-License-Identifier: AGPL-3.0-or-later
//! Output encoding and soft-clip pins a rendered score compares.

use looklab_color::ucs::Ucs;
use looklab_color::{Mat3, RgbSpace};
use looklab_schema::{OutputEncoding, ScoringMetric};
use serde::{Deserialize, Serialize};

use crate::clip::{output_encode_clip, try_output_encode_clip};
use crate::composite::{CompositeDerivative, evaluate as evaluate_composite};
use crate::error::{ModelError, Result};
use crate::file::{LookDocument, OutputEncoding as FileOutputEncoding};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "OutputPinWire")]
pub struct OutputPin {
    pub encoding: OutputEncoding,
    pub clip_k: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputPinWire {
    encoding: OutputEncoding,
    clip_k: f64,
}

impl TryFrom<OutputPinWire> for OutputPin {
    type Error = ModelError;

    fn try_from(value: OutputPinWire) -> Result<Self> {
        Self::new(value.encoding, value.clip_k)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RenderJacobian {
    Finite(Mat3),
    DomainViolation,
    Singular,
}

impl OutputPin {
    /// # Errors
    /// A clip outside the published range or a non-finite value.
    pub fn new(encoding: OutputEncoding, clip_k: f64) -> Result<Self> {
        if !clip_k.is_finite() || !(0.005..=0.1).contains(&clip_k) {
            return Err(ModelError::Invalid(format!(
                "output clip {clip_k} is outside 0.005..=0.1"
            )));
        }
        Ok(Self { encoding, clip_k })
    }

    /// # Errors
    /// A clip outside the published range.
    pub fn from_document(document: &LookDocument) -> Result<Self> {
        let encoding = match document
            .file
            .definition
            .as_ref()
            .map(|definition| definition.output)
        {
            Some(FileOutputEncoding::Srgb) | None => OutputEncoding::Srgb,
        };
        Self::new(encoding, document.file.output_clip.k)
    }

    pub const fn space(self) -> RgbSpace {
        match self.encoding {
            OutputEncoding::Srgb => RgbSpace::Srgb,
            OutputEncoding::AdobeRgb => RgbSpace::AdobeRgb,
            OutputEncoding::Rec2020 => RgbSpace::Rec2020,
            OutputEncoding::DisplayP3 => RgbSpace::DisplayP3,
            OutputEncoding::ProPhoto => RgbSpace::ProPhoto,
        }
    }

    pub fn render(self, pre: Ucs) -> Ucs {
        output_encode_clip(pre, self.space(), self.clip_k)
    }

    pub fn try_render(self, pre: Ucs) -> Option<Ucs> {
        try_output_encode_clip(pre, self.space(), self.clip_k)
    }

    pub fn render_jacobian_status(self, pre: Ucs) -> RenderJacobian {
        match evaluate_composite(pre, self.space(), self.clip_k) {
            Ok(point) => match point.derivative {
                CompositeDerivative::Finite { jacobian, .. } => {
                    RenderJacobian::Finite(Mat3(jacobian))
                }
                CompositeDerivative::Singular(_) => RenderJacobian::Singular,
            },
            Err(_) => RenderJacobian::DomainViolation,
        }
    }

    /// `∂rendered/∂pre`; `None` when the pre-clip inverse is infeasible, the
    /// encode falls back to the neutral, or the composite is singular.
    pub fn render_jacobian(self, pre: Ucs) -> Option<Mat3> {
        match self.render_jacobian_status(pre) {
            RenderJacobian::Finite(jacobian) => Some(jacobian),
            RenderJacobian::DomainViolation | RenderJacobian::Singular => None,
        }
    }

    pub fn canonical(self) -> String {
        format!("{}@{}", self.encoding.as_str(), self.clip_k)
    }

    /// # Errors
    /// Text that is not `<encoding>@<clip>`.
    pub fn parse(value: &str) -> Result<Self> {
        let (encoding, clip) = value
            .split_once('@')
            .ok_or_else(|| ModelError::Invalid(format!("output pin {value:?}")))?;
        let encoding = OutputEncoding::parse(encoding)
            .map_err(|error| ModelError::Invalid(error.to_string()))?;
        let clip = clip
            .parse()
            .map_err(|_| ModelError::Invalid(format!("output clip {clip:?}")))?;
        Self::new(encoding, clip)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputContext {
    pub target: OutputPin,
    pub camera: OutputPin,
}

impl OutputContext {
    pub const fn metric(&self) -> ScoringMetric {
        ScoringMetric::RenderedPostClipV1
    }

    pub fn canonical(&self) -> String {
        format!(
            "target={};camera={}",
            self.target.canonical(),
            self.camera.canonical()
        )
    }

    /// # Errors
    /// Text that is not `target=<pin>;camera=<pin>`.
    pub fn parse(value: &str) -> Result<Self> {
        let mut target = None;
        let mut camera = None;
        for part in value.split(';') {
            let (key, pin) = part
                .split_once('=')
                .ok_or_else(|| ModelError::Invalid(format!("output context {part:?}")))?;
            match key {
                "target" => {
                    if target.is_some() {
                        return Err(ModelError::Invalid("duplicate output target".to_string()));
                    }
                    target = Some(OutputPin::parse(pin)?);
                }
                "camera" => {
                    if camera.is_some() {
                        return Err(ModelError::Invalid("duplicate output camera".to_string()));
                    }
                    camera = Some(OutputPin::parse(pin)?);
                }
                other => {
                    return Err(ModelError::Invalid(format!(
                        "unknown output context field {other:?}"
                    )));
                }
            }
        }
        Ok(Self {
            target: target.ok_or_else(|| ModelError::Invalid("output target".to_string()))?,
            camera: camera.ok_or_else(|| ModelError::Invalid("output camera".to_string()))?,
        })
    }
}
