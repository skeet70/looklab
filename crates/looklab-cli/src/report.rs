// SPDX-License-Identifier: AGPL-3.0-or-later
//! `looklab report`: one static HTML page of Look diagnostics.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use looklab_model::probes;
use looklab_report::{ReportSource, look_diagnostics, look_page};

use crate::util;

#[derive(Debug, Args)]
pub struct ReportArgs {
    /// `.looklab` file to report.
    pub look: PathBuf,
    #[arg(long)]
    pub out: PathBuf,
}

pub fn run(args: &ReportArgs) -> Result<()> {
    let document = util::load_document(&args.look)?;
    let probes = probes::v1();
    let diagnostics = look_diagnostics(&document.look, &probes);
    let source = ReportSource::from_document(&document);
    let page = look_page(&source, &diagnostics);
    let written = crate::write(&args.out, page.as_bytes())?;
    println!("report: {} ({} bytes)", args.out.display(), written);
    Ok(())
}
