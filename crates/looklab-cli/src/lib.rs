// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native command-line tool: prep, fit, report, compare, export, validate,
//! synth, verify-payload, admin.
//! Commands and formats follow docs/01 "The CLI", docs/03 §2-§3, docs/04 §7.

mod admin;
mod body_transform;
mod card;
mod compare;
mod export;
mod fit;
mod lut_look;
mod match_cmd;
mod prep;
mod publish;
mod report;
mod synth;
mod util;
mod validate;
mod verify_payload;

pub use body_transform::{BodyTransform, TransformFile};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "looklab",
    version,
    about = "Measure, compare and reproduce camera looks."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Grant, revoke or list tiers and roles without the server.
    Admin(admin::AdminArgs),
    /// Pair a RAW with its JPEGs (or a folder), prepare samples and write blobs.
    Prep(prep::PrepArgs),
    /// Fit one Look from a directory of `.llsamp` blobs.
    Fit(fit::FitArgs),
    /// Write an HTML report for one Look.
    Report(report::ReportArgs),
    /// Compare two Looks and write an HTML comparison.
    Compare(compare::CompareArgs),
    /// Bake a Look for a software interface profile and validate the artifact.
    Export(export::ExportArgs),
    /// Inspect or evaluate a firmware card file.
    Card(card::CardArgs),
    /// Import a community LUT.
    LutLook(lut_look::LutLookArgs),
    /// Validate a data directory and its probe set files.
    Validate(validate::ValidateArgs),
    /// Validate a community data snapshot directory and its content hash.
    ValidateSnapshot(validate::ValidateSnapshotArgs),
    /// Write a synthetic dataset so the whole flow runs without fixtures.
    Synth(synth::SynthArgs),
    /// Reproduce a browser upload from a HAR under wasmtime and compare it byte for byte.
    VerifyPayload(verify_payload::VerifyPayloadArgs),
    /// Recommend camera settings for a target Look and print the settings card.
    Match(match_cmd::MatchArgs),
    /// Run one verification-loop iteration from observed re-renders.
    Verify(match_cmd::VerifyArgs),
    /// Render the static site from the server database.
    Publish(publish::PublishArgs),
}

pub fn run() -> Result<()> {
    run_with(Cli::parse())
}

pub fn run_with(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Admin(args) => admin::run(&args),
        Command::Prep(args) => prep::run(&args),
        Command::Fit(args) => fit::run(&args),
        Command::Report(args) => report::run(&args),
        Command::Compare(args) => compare::run(&args),
        Command::Export(args) => export::run(&args),
        Command::Card(args) => card::run(&args),
        Command::LutLook(args) => lut_look::run(&args),
        Command::Validate(args) => validate::run(&args),
        Command::ValidateSnapshot(args) => validate::run_snapshot(&args),
        Command::Synth(args) => synth::run(&args),
        Command::VerifyPayload(args) => verify_payload::run(&args),
        Command::Match(args) => match_cmd::run_match(&args),
        Command::Verify(args) => match_cmd::run_verify(&args),
        Command::Publish(args) => publish::run(&args),
    }
}

fn write(path: &std::path::Path, bytes: &[u8]) -> Result<u64> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(bytes.len() as u64)
}
