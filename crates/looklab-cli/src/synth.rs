// SPDX-License-Identifier: AGPL-3.0-or-later
//! `looklab synth`: a synthetic dataset for the whole flow.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use looklab_fit::synth::{SynthConfig, dataset};
use looklab_samples::encode;

use crate::body_transform::{PerImageTransform, TransformFile};

#[derive(Debug, Args)]
pub struct SynthArgs {
    #[arg(long)]
    pub out: PathBuf,
    #[arg(long, default_value_t = 0x5eed_1234)]
    pub seed: u64,
    #[arg(long, default_value_t = 8)]
    pub images: usize,
    #[arg(long, default_value_t = 500)]
    pub samples: usize,
}

pub fn run(args: &SynthArgs) -> Result<()> {
    let config = SynthConfig {
        seed: args.seed,
        images: args.images,
        samples_per_image: args.samples,
        ..SynthConfig::default()
    };
    let dataset = dataset(&config);

    let mut per_image = Vec::with_capacity(dataset.images.len());
    for (index, image) in dataset.images.iter().enumerate() {
        let name = format!("image-{index:03}.llsamp");
        let bytes = encode(&image.samples).with_context(|| format!("encoding {name}"))?;
        crate::write(&args.out.join(&name), &bytes)?;
        per_image.push(PerImageTransform {
            file: name,
            camera_to_xyz: image.transform,
        });
    }

    let transform = TransformFile {
        format: crate::body_transform::FORMAT.to_owned(),
        version: 1,
        body: Some("synthetic/looklab".to_owned()),
        cct: None,
        camera_to_xyz: dataset.images.first().map(|image| image.transform),
        color_matrices: Vec::new(),
        forward_matrices: Vec::new(),
        per_image,
    };
    crate::write(
        &args.out.join("transform.yaml"),
        transform.to_yaml()?.as_bytes(),
    )?;
    println!(
        "synth: {} image(s) in {}",
        dataset.images.len(),
        args.out.display()
    );
    Ok(())
}
