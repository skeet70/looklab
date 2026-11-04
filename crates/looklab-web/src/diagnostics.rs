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
        let covariance = self.document.uncertainty.probe_cov.as_ref()?;
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
        let Some(density) = self
            .document
            .uncertainty
            .density
            .as_ref()
            .and_then(|density| density.values.get(position))
        else {
            return Support::Supported;
        };
        let sigma = self.covariance(probe).map_or(0.0, Covariance3::sigma);
        if sigma <= SUPPORT_SIGMA && *density > 0.0 {
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
