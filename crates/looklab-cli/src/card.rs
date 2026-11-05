// SPDX-License-Identifier: AGPL-3.0-or-later
//! Firmware card files (docs/04 §8).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use looklab_export::card::{Card, read_card};

#[derive(Debug, Args)]
pub struct CardArgs {
    #[command(subcommand)]
    pub command: CardCommand,
}

#[derive(Debug, Subcommand)]
pub enum CardCommand {
    /// Print the header, the section table and the metadata of a card.
    Inspect { file: PathBuf },
    /// Evaluate scene-linear Rec.2020 RGB triples through the card grid.
    Eval {
        file: PathBuf,
        /// Linear Rec.2020 values, three per colour.
        #[arg(required = true, num_args = 3.., value_name = "RGB")]
        rgb: Vec<f64>,
    },
}

pub fn run(args: &CardArgs) -> Result<()> {
    match &args.command {
        CardCommand::Inspect { file } => inspect(file),
        CardCommand::Eval { file, rgb } => eval(file, rgb),
    }
}

fn load(path: &Path) -> Result<(Card, usize)> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let card = read_card(&bytes).map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
    Ok((card, bytes.len()))
}

fn inspect(path: &Path) -> Result<()> {
    let (card, file_length) = load(path)?;
    println!("file {} ({} bytes)", path.display(), file_length);
    println!(
        "header: magic LLCF, version 1, header 64 bytes, {} sections",
        card.sections().len()
    );
    println!("content id: {}", card.content_id_hex());
    println!("cid prefix: {}", hex(&card.cid_prefix()));
    println!("sections:");
    for section in card.sections() {
        println!(
            "  {:>4} {:<9} offset {:>8} length {:>8}",
            section.kind,
            kind_name(section.kind),
            section.offset,
            section.length
        );
    }
    let meta = card.meta();
    println!("meta:");
    println!("  slug: {}", meta.slug);
    println!("  version: {}", meta.version);
    println!("  name: {}", meta.name);
    println!("  short name: {}", meta.short_name);
    if let Some(description) = &meta.description {
        println!("  description: {description}");
    }
    if let Some(license) = &meta.license {
        println!("  license: {license}");
    }
    println!(
        "semantics: input scene-linear Rec.2020, output reference pre-clip Rec.2020, shaper log2, viewing reference, wb adaptive"
    );
    println!("neutral: {} samples", card.neutral().len());
    println!("fit: {} records", card.fit().len());
    match card.grid() {
        Some(grid) => println!("grid: {}^3", grid.side().side()),
        None => println!("grid: none"),
    }
    Ok(())
}

fn eval(path: &Path, values: &[f64]) -> Result<()> {
    if !values.len().is_multiple_of(3) {
        bail!("give RGB triples, three values per colour");
    }
    let (card, _) = load(path)?;
    if card.grid().is_none() {
        bail!("the card has no GRID section");
    }
    let (triples, _) = values.as_chunks::<3>();
    for triple in triples {
        let input = [triple[0], triple[1], triple[2]];
        let Some(output) = card.eval_grid(input) else {
            bail!("cannot evaluate a non-finite input");
        };
        println!(
            "in [{:.6}, {:.6}, {:.6}] -> [{:.9}, {:.9}, {:.9}]",
            input[0], input[1], input[2], output[0], output[1], output[2]
        );
    }
    Ok(())
}

fn kind_name(kind: u32) -> &'static str {
    match kind {
        1 => "META",
        2 => "SEMANTICS",
        3 => "NEUTRAL",
        4 => "FIT",
        5 => "GRID",
        _ => "UNKNOWN",
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
