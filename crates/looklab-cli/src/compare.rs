// SPDX-License-Identifier: AGPL-3.0-or-later
//! `looklab compare`: the probe-set comparison of two Looks.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use looklab_color::Xyz;
use looklab_color::ucs::Ucs;
use looklab_model::probes;
use looklab_model::{LayerValues, LookDocument, LookInput, Probe, ProbeSet};
use looklab_report::{
    ComparisonInput, Covariance3, FunctionName, LookFunction, ReportSource, Support, compare,
    comparison_page,
};

use crate::util;

#[derive(Debug, Args)]
pub struct CompareArgs {
    /// First `.looklab` file.
    pub a: PathBuf,
    /// Second `.looklab` file.
    pub b: PathBuf,
    #[arg(long)]
    pub out: PathBuf,
    /// Also write `comparison.json` beside the HTML file.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: &CompareArgs) -> Result<()> {
    let a = util::load_document(&args.a)?;
    let b = util::load_document(&args.b)?;
    let probes = probes::v1();

    let a_function = DocumentFunction::new(&a, &probes);
    let b_function = DocumentFunction::new(&b, &probes);
    let comparison = compare(&ComparisonInput {
        a: &a_function,
        b: &b_function,
        probes: &probes,
        a_name: a_function.name(),
        b_name: b_function.name(),
    });

    let page = comparison_page(
        &comparison,
        &ReportSource::from_document(&a),
        &ReportSource::from_document(&b),
    );
    let written = crate::write(&args.out, page.as_bytes())?;
    if args.json {
        let json_path = util::replace_extension(&args.out, "", "json");
        let json = serde_json::to_string_pretty(&comparison)?;
        crate::write(&json_path, json.as_bytes())?;
        eprintln!("comparison JSON: {}", json_path.display());
    }
    eprintln!(
        "comparison HTML: {} ({} bytes)",
        args.out.display(),
        written
    );
    println!("{}", comparison.summary);
    Ok(())
}

struct DocumentFunction<'a> {
    document: &'a LookDocument,
    probes: &'a ProbeSet,
    name: FunctionName,
}

impl<'a> DocumentFunction<'a> {
    fn new(document: &'a LookDocument, probes: &'a ProbeSet) -> Self {
        let identity = &document.file.look;
        Self {
            document,
            probes,
            name: FunctionName::new(format!(
                "{} {}",
                identity.slug.as_str(),
                identity.version.as_str()
            )),
        }
    }

    fn name(&self) -> &FunctionName {
        &self.name
    }

    fn probe_index(&self, probe: &Probe) -> Option<usize> {
        self.probes
            .probes
            .iter()
            .position(|candidate| candidate.id == probe.id)
    }
}

impl LookFunction for DocumentFunction<'_> {
    fn predict(&self, xyz: Xyz) -> Ucs {
        self.document.look.eval(xyz)
    }

    fn covariance(&self, probe: &Probe) -> Option<Covariance3> {
        let covariance = self.document.uncertainty.probe_cov.as_ref()?;
        let index = self.probe_index(probe)?;
        let values = covariance.values.get(index * 9..index * 9 + 9)?;
        Some(Covariance3 {
            jj: values[0],
            ja: values[1],
            jb: values[2],
            aa: values[4],
            ab: values[5],
            bb: values[8],
        })
    }

    fn support(&self, _probe: &Probe) -> Support {
        Support::Supported
    }

    fn layer_values(&self, input: &LookInput) -> Option<LayerValues> {
        Some(self.document.look.layer_values(input))
    }

    fn look(&self) -> Option<&looklab_model::Look> {
        Some(&self.document.look)
    }
}
