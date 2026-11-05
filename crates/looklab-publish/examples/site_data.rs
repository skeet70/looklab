// SPDX-License-Identifier: AGPL-3.0-or-later
//! Generates sample Look fits, camera match bundles, CC0 photo scenes, test charts, bodies and an audit log.
//! The first argument selects the output directory; `--charts-only` omits photographs.
#![allow(clippy::disallowed_methods)]

use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use looklab_export::card::{CardMeta, CardOptions, write_card};
use looklab_fit::synth::{SynthConfig, SyntheticDataset, dataset};
use looklab_fit::{FitOptions, fit};
use looklab_match::{ControlCovariance, synth};
use looklab_model::file::{
    BodyProvenance, LookIdentity, LookKind, Provenance, RootProvenance, WbPolicy,
};
use looklab_model::realization::{ControlResponseModel, encode_crm};
use looklab_model::{
    Density, Look, LookDocument, ProbeCovariance, ProbeScalars, Tone, UncertaintyData,
};
use looklab_publish::{
    ArtifactBlob, ArtifactHash, ArtifactName, AuditEntry, AuditEvent, AuditLog, BodyExtras,
    BodyPage, BodyUse, CARD_ARTIFACT, CredentialRef, CredentialTier, DOCUMENT_ARTIFACT, Decision,
    GateCheck, GateSummary, InputTransformPage, LicensePage, LookPage, LookRef, LookStatus,
    LookVersionPage, PREVIEW_LUT_ARTIFACT, ProposalRef, ProvenanceSummary, RealizationNote, Role,
    Scene, SceneName, SiteData, SitePath, Timestamp, TransformSource, VersionNotes, VersionRef,
    WeakSpot, bake_preview_lut, write_site,
};
use looklab_report::compare::{ComparisonInput, FunctionName, compare};
use looklab_schema::{validate_body, validate_control_schema};

const SIZE: u32 = 1414;
const CREATED: &str = "2026-11-04T00:00:00Z";

fn main() -> Result<(), Box<dyn Error>> {
    let out = PathBuf::from(
        std::env::args()
            .skip(1)
            .find(|arg| !arg.starts_with("--"))
            .unwrap_or_else(|| "site".to_owned()),
    );
    fs::create_dir_all(&out)?;

    let body = source_body()?;
    let mut bodies = vec![body];
    if let Some(mut second) = bodies[0].clone().into_second_body() {
        second.mapping_status = looklab_publish::MappingStatus::Provisional;
        second.input_transform = Some(InputTransformPage {
            version: 1,
            source: TransformSource::Chart,
            anchor_count: 3,
            tier: CredentialTier::New,
        });
        second.realization_notes = vec![RealizationNote {
            epoch: None,
            text: "We still need a sample file to confirm this camera's menu choices.".to_owned(),
        }];
        bodies.push(second);
    }

    let looks = synthetic_looks(&bodies)?;
    for body in &mut bodies {
        for card in &mut body.native_looks {
            if let Some(look) = looks.get(if card.sim_key.as_str() == "classic_chrome" {
                0
            } else {
                1
            }) {
                card.look = look.slug.clone();
            }
        }
        for epoch in &mut body.epochs {
            for sim in &mut epoch.native_sims {
                if let Some(look) = looks.get(if sim.key.as_str() == "classic_chrome" {
                    0
                } else {
                    1
                }) {
                    sim.look = look.slug.clone();
                }
            }
        }
    }
    let mut audit = vec![
        AuditEntry {
            seq: 1,
            ts: Timestamp::new(CREATED)?,
            event: AuditEvent::Grant {
                subject: CredentialRef::new("cred-7f21")?,
                role: Role::Moderator,
                granted_by: CredentialRef::new("cli")?,
                reason: "moderation queue bootstrap".to_owned(),
            },
        },
        AuditEntry {
            seq: 2,
            ts: Timestamp::new(CREATED)?,
            event: AuditEvent::Endorsement {
                issuer: CredentialRef::new("cred-19aa")?,
                subject: CredentialRef::new("cred-7f21")?,
                reason: "chart sessions reviewed".to_owned(),
            },
        },
        AuditEntry {
            seq: 3,
            ts: Timestamp::new(CREATED)?,
            event: AuditEvent::ModerationDecision {
                proposal: ProposalRef::new("proposal-4")?,
                moderator: CredentialRef::new("cred-7f21")?,
                decision: Decision::Approve,
                reason: "round-trip report matches the submission".to_owned(),
            },
        },
    ];
    let mut seq = audit.len() as u64;
    for look in &looks {
        for version in &look.versions {
            let manifest = version.manifest.to_json()?;
            seq += 1;
            audit.push(AuditEntry {
                seq,
                ts: Timestamp::new(CREATED)?,
                event: AuditEvent::Release {
                    look: LookRef::new(look.slug.as_str())?,
                    version: VersionRef::new(version.version().as_str())?,
                    manifest_sha256: ArtifactHash::of(&manifest),
                },
            });
        }
    }

    let site = SiteData {
        bodies,
        looks,
        audit_log: AuditLog::new(audit),
        scenes: canonical_scenes()?,
    };

    let mut sink = |path: &SitePath, bytes: &[u8]| -> std::io::Result<()> {
        let full = out.join(path.as_str());
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(full, bytes)
    };
    let root = write_site(&mut sink, &site)?;
    fs::write(out.join("site-root.txt"), root.as_str())?;
    publish_conformance_archive(&out)?;
    println!(
        "wrote {}/data ({} bytes of index)",
        out.display(),
        fs::read(out.join("data/index.json"))
            .map(|bytes| bytes.len())
            .unwrap_or(0)
    );
    Ok(())
}

#[derive(serde::Serialize)]
struct ConformanceArchive {
    archive: SitePath,
    sha256: ArtifactHash,
}

fn publish_conformance_archive(out: &Path) -> Result<(), Box<dyn Error>> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/card/conformance-v1");
    let archive = SitePath::new("data/developers/conformance-v1.zip")?;
    let destination = out.join(archive.as_str());
    fs::create_dir_all(out.join("data/developers"))?;
    let mut writer = zip::ZipWriter::new(fs::File::create(&destination)?);
    archive_conformance_directory(&mut writer, &source, &source)?;
    writer.finish()?;
    let metadata = ConformanceArchive {
        archive,
        sha256: ArtifactHash::of(&fs::read(&destination)?),
    };
    fs::write(
        out.join("data/developers/conformance-v1.json"),
        serde_json::to_vec_pretty(&metadata)?,
    )?;
    Ok(())
}

fn archive_conformance_directory(
    writer: &mut zip::ZipWriter<fs::File>,
    root: &Path,
    directory: &Path,
) -> Result<(), Box<dyn Error>> {
    let mut entries = fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(0o644);
    for path in entries {
        let kind = fs::symlink_metadata(&path)?.file_type();
        if kind.is_dir() {
            archive_conformance_directory(writer, root, &path)?;
        } else if kind.is_file() {
            let relative = path.strip_prefix(root)?;
            let name = relative
                .to_str()
                .ok_or("conformance file name is not UTF-8")?;
            writer.start_file(format!("conformance-v1/{name}"), options)?;
            writer.write_all(&fs::read(path)?)?;
        } else {
            return Err(format!("unsupported conformance entry: {}", path.display()).into());
        }
    }
    Ok(())
}

fn source_body() -> Result<BodyPage, Box<dyn Error>> {
    let body = validate_body(include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml"))
        .map_err(|errors| format!("body: {errors:?}"))?;
    let schema = validate_control_schema(include_str!("../../../data/bodies/fujifilm-x-t5.yaml"))
        .map_err(|errors| format!("schema: {errors:?}"))?;
    let extras = BodyExtras {
        input_transform: Some(InputTransformPage {
            version: 7,
            source: TransformSource::Chart,
            anchor_count: 12,
            tier: CredentialTier::Endorsed,
        }),
        realization_notes: vec![
            RealizationNote {
                epoch: Some(1),
                text: "Renders about 0.5 EV darker in DR200 than the reference camera.".to_owned(),
            },
            RealizationNote {
                epoch: Some(2),
                text: "Brightness stays the same across firmware versions; Color Chrome Blue changes the shadows.".to_owned(),
            },
        ],
    };
    Ok(BodyPage::from_schema(&body, &schema, &extras)?)
}

fn synthetic_looks(bodies: &[BodyPage]) -> Result<Vec<LookPage>, Box<dyn Error>> {
    let specs = [
        (
            11_u64,
            "sunlit-negative",
            "Sunlit Negative",
            "Warm negative film: lifted shadows, gentle highlight rolloff.",
            SyntheticLook::SunlitNegative,
            CoverageGap::DeepShadows,
            "Shadow separation is weaker than the reference body.",
            None,
        ),
        (
            22,
            "cold-steel",
            "Cold Steel",
            "Cool color, stronger contrast and restrained saturation.",
            SyntheticLook::ColdSteel,
            CoverageGap::SkinMidtones,
            "Midtone skin drifts slightly green at low chroma.",
            None,
        ),
        (
            33,
            "neutral-studio",
            "Neutral Studio",
            "Near-neutral studio color with gentle contrast for product and copy work.",
            SyntheticLook::NeutralStudio,
            CoverageGap::SaturatedReds,
            "Red highlights reach the soft clip earlier than the source scene.",
            Some("This version was produced by a revert to an earlier fit; the excluded credential is listed in the audit log.".to_owned()),
        ),
    ];
    let mut looks = Vec::new();
    for (seed, slug, name, description, character, region, note, notice) in specs {
        let data = covered_dataset(seed, character, region)?;
        let result = fit(
            &data.images,
            &FitOptions {
                seed,
                max_sweeps: 12,
                max_samples_per_image: 2_000,
                hessian_samples: 8_000,
                sweep_budget: 3_000_000.0,
                run_gcv: true,
                fit_residual: false,
                // These captures have calibrated exposure, WB and output clipping.
                exposure_prior_sd: 0.0001,
                wb_prior_sd: 0.0001,
                exposure_mode_prior_sd: 0.0001,
                clip_prior: (data.clip_k, 0.0001),
                ..FitOptions::default()
            },
        )?;
        report_recovery(slug, &data, &result, region);
        let probes = &result.probes.probes;
        let supported = probes.iter().filter(|probe| probe.supported).count();
        println!(
            "{slug}: supported {}/{}; sigma {}, density {}, clusters {}, bias {}",
            supported,
            probes.len(),
            probes.iter().filter(|p| p.supported_sigma).count(),
            probes.iter().filter(|p| p.supported_density).count(),
            probes.iter().filter(|p| p.supported_clusters).count(),
            probes.iter().filter(|p| p.supported_bias).count(),
        );
        if supported * 2 <= probes.len() {
            return Err(format!(
                "{slug}: only {supported}/{} probes are supported",
                probes.len()
            )
            .into());
        }
        let mut document = LookDocument::from_look(
            LookIdentity::new(slug, "1.0.0", LookKind::NativeFamily, WbPolicy::Adaptive)?,
            result.look.clone(),
        )?;
        let uncertainty = uncertainty(&result)?;
        document = document.with_uncertainty(uncertainty)?;
        document.file.provenance = provenance(&data, &result)?;
        let look_bytes = document.encode()?;
        let lut = bake_preview_lut(&document);
        let card = write_card(
            &document,
            &CardMeta::new(name, name)
                .with_description(description)
                .with_license("Project-owned synthetic data"),
            &CardOptions::default(),
        )?;

        let mut artifacts = vec![
            ArtifactBlob::new(ArtifactName::new(DOCUMENT_ARTIFACT)?, look_bytes),
            ArtifactBlob::new(ArtifactName::new(PREVIEW_LUT_ARTIFACT)?, lut),
            ArtifactBlob::new(ArtifactName::new(CARD_ARTIFACT)?, card),
        ];
        artifacts.extend(sample_match_artifacts(&document, bodies)?);
        let mut gate_summary = gates()?;
        gate_summary.checks.push(GateCheck {
            name: "Sample settings models".into(),
            passed: true,
            detail: Some("Synthetic camera response data for UI demonstrations, not measured camera recipes.".into()),
        });
        let slug = looklab_model::file::Slug::new(slug)?;
        let version = LookVersionPage::new(
            slug.clone(),
            looklab_model::file::LookVersion::new("1.0.0")?,
            LookStatus::Published,
            gate_summary,
            provenance_summary(&data),
            artifacts,
            VersionNotes {
                changelog: vec![format!(
                    "GCV fit of {name} from {} exposure/WB-calibrated probe-domain captures with balanced gray ramps and known output clipping, rendered through its distinct synthetic truth: {description} Coverage excludes {} (seed {seed}).",
                    data.images.len(),
                    region.label()
                )],
                weak_spots: vec![WeakSpot {
                    region: region.label().to_owned(),
                    note: note.to_owned(),
                    bounty: Some("bounty-1".to_owned()),
                }],
                notice,
            },
        )?;
        looks.push(LookPage {
            slug,
            name: name.to_owned(),
            kind: LookKind::NativeFamily,
            wb_policy: WbPolicy::Adaptive,
            description: description.to_owned(),
            aliases: Vec::new(),
            license: Some(LicensePage {
                declaration: "Project-owned synthetic data".to_owned(),
                attribution: "looklab".to_owned(),
            }),
            versions: vec![version],
        });
    }
    Ok(looks)
}

fn sample_match_artifacts(
    target: &LookDocument,
    bodies: &[BodyPage],
) -> Result<Vec<ArtifactBlob>, Box<dyn Error>> {
    let synthetic = synth::body(9)?;
    let base =
        LookDocument::from_look(target.file.look.clone(), synthetic.model.base_look.clone())?;
    let base_bytes = base.encode()?;
    let transform = serde_norway::to_string(&TransformOut {
        format: "looklab-transform/1",
        version: 1,
        camera_to_xyz: synth::transform(),
    })?;
    let mut artifacts = Vec::new();
    for camera in bodies
        .iter()
        .filter(|body| body.kind == looklab_publish::BodyKind::Camera)
    {
        let schema =
            validate_control_schema(&camera.schema).map_err(|error| format!("{error:?}"))?;
        for epoch in &camera.epochs {
            let sim = epoch
                .native_sims
                .first()
                .ok_or("Sample camera has no built-in look.")?;
            let mut crm = ControlResponseModel::zeroed(&schema, epoch.id, &sim.key, 1)?;
            if crm.body == synthetic.model.crm.body && epoch.id == synthetic.model.crm.epoch {
                crm = synthetic.model.crm.clone();
            }
            let ids = crm.parameter_ids();
            let n = ids.len();
            let mut covariance = vec![0.0_f64; n * n];
            for index in 0..n {
                covariance[index * n + index] = 1.0e-4;
            }
            let covariance = ControlCovariance {
                parameter_ids: ids,
                covariance,
            }
            .to_json()?;
            let crm = encode_crm(&crm)?;
            let prefix = format!(
                "match-{}-{}-",
                camera.id.as_str().replace('/', "-"),
                epoch.id
            );
            for (suffix, bytes) in [
                ("base.looklab", base_bytes.clone()),
                ("body.yaml", camera.schema.as_bytes().to_vec()),
                ("crm.json", crm),
                ("covariance.json", covariance),
                ("transform.yaml", transform.as_bytes().to_vec()),
            ] {
                artifacts.push(ArtifactBlob::new(
                    ArtifactName::new(format!("{prefix}{suffix}"))?,
                    bytes,
                ));
            }
        }
    }
    Ok(artifacts)
}

#[derive(serde::Serialize)]
struct TransformOut {
    format: &'static str,
    version: u32,
    camera_to_xyz: looklab_color::Mat3,
}

#[derive(Clone, Copy)]
enum SyntheticLook {
    SunlitNegative,
    ColdSteel,
    NeutralStudio,
}

impl SyntheticLook {
    fn truth(self) -> Result<Look, Box<dyn Error>> {
        use looklab_model::basis::TONE_KNOTS;

        let mut look = Look::colorimetric_spline()?;
        let reference_mid = look.tone.eval(0.0);
        let Tone::Spline { c0, mut beta } = look.tone else {
            return Err("synthetic truth requires a spline tone".into());
        };
        let (contrast, tint_a, tint_b, log_gain) = match self {
            Self::SunlitNegative => (0.84, 1.4, 4.0, -0.04),
            Self::ColdSteel => (1.16, -0.8, -3.8, -0.24),
            Self::NeutralStudio => (0.98, 0.0, 0.0, -0.01),
        };
        for (index, slope) in beta.iter_mut().enumerate() {
            let s = (TONE_KNOTS[index + 1] + TONE_KNOTS[index + 2] + TONE_KNOTS[index + 3]) / 3.0;
            let shoulder = match self {
                Self::SunlitNegative => 1.0 - 0.3 * ((s - 0.5) / 3.0).clamp(0.0, 1.0),
                Self::ColdSteel | Self::NeutralStudio => 1.0,
            };
            *slope *= contrast * shoulder;
        }
        let shaped = Tone::spline(c0, beta)?;
        look.tone = Tone::spline(c0 + reference_mid - shaped.eval(0.0), beta)?;
        look.tint.a.fill(tint_a);
        look.tint.b.fill(tint_b);
        // Uniform chroma gain is a global mode of the smoothness penalty.
        look.color.g.plane.fill(log_gain);
        look.color.g.volume.fill(log_gain);
        look.validate()?;
        Ok(look)
    }
}

fn report_recovery(
    slug: &str,
    data: &SyntheticDataset,
    result: &looklab_fit::FitResult,
    gap: CoverageGap,
) {
    let probes = looklab_model::probes::v1();
    let reference = Look::colorimetric();
    let reference_name = FunctionName::new("Neutral color");
    for (label, look) in [("truth", &data.truth), ("recovered", &result.look)] {
        let name = FunctionName::new(format!("{slug} {label}"));
        let report = compare(&ComparisonInput {
            a: &reference,
            b: look,
            probes: &probes,
            a_name: &reference_name,
            b_name: &name,
        });
        println!("{}", report.summary);
    }
    let mut errors = Vec::with_capacity(probes.probes.len());
    for probe in &probes.probes {
        if gap.excludes(probe.xyz) {
            continue;
        }
        let truth = looklab_fit::post_clip(
            &data.truth,
            probe.xyz,
            looklab_color::RgbSpace::Srgb,
            data.clip_k,
        );
        let recovered = looklab_fit::post_clip(
            &result.look,
            probe.xyz,
            looklab_color::RgbSpace::Srgb,
            result.realization.clip_k,
        );
        errors.push(looklab_color::ucs::delta_e_ucs(truth, recovered));
    }
    errors.sort_by(f64::total_cmp);
    let mean = errors.iter().sum::<f64>() / errors.len() as f64;
    let p95 = errors[(errors.len() - 1) * 95 / 100];
    println!(
        "{slug}: truth-vs-fit post-clip UCS on {} covered probes: mean {mean:.3}, p95 {p95:.3}; GCV {:?}",
        errors.len(),
        result.lambdas,
    );
}

#[derive(Clone, Copy)]
enum CoverageGap {
    DeepShadows,
    SkinMidtones,
    SaturatedReds,
}

impl CoverageGap {
    fn label(self) -> &'static str {
        match self {
            Self::DeepShadows => "deep shadows",
            Self::SkinMidtones => "skin midtones",
            Self::SaturatedReds => "saturated reds",
        }
    }

    fn excludes(self, xyz: looklab_color::Xyz) -> bool {
        let input = looklab_model::LookInput::new(xyz);
        match self {
            Self::DeepShadows => input.s < -5.5,
            Self::SkinMidtones => {
                (-0.5..0.5).contains(&input.s)
                    && (25.0..75.0).contains(&input.h_in)
                    && (5.0..35.0).contains(&input.c_in)
            }
            Self::SaturatedReds => {
                input.s > 0.5 && (input.h_in < 30.0 || input.h_in > 345.0) && input.c_in > 20.0
            }
        }
    }
}

fn covered_dataset(
    seed: u64,
    character: SyntheticLook,
    gap: CoverageGap,
) -> Result<SyntheticDataset, Box<dyn Error>> {
    use looklab_color::ucs::{Ucs, UcsSpace, ucs_to_jmh};
    use looklab_model::probes::ProbeGroup;
    use looklab_model::solve::reference_model;
    use looklab_samples::{SampleFlags, SampleSet};
    let mut data = dataset(&SynthConfig {
        seed,
        images: 32,
        samples_per_image: 1,
        exposure_sd: 0.0,
        wb_sd: 0.0,
        hint_sd: 0.0,
        ..SynthConfig::default()
    });
    data.truth = character.truth()?;
    let probes = looklab_model::probes::v1();
    let covered: Vec<_> = probes
        .probes
        .iter()
        .enumerate()
        .filter(|(_, probe)| !gap.excludes(probe.xyz))
        .collect();
    let gray_count = covered
        .iter()
        .filter(|(_, probe)| probe.group == ProbeGroup::GrayRamp)
        .count();
    let color_count = covered.len() - gray_count;
    let gray_observations = color_count.div_ceil(3 * gray_count);
    let samples_per_image = color_count + gray_count * gray_observations;
    println!(
        "seed {seed}: {samples_per_image} samples/capture, {} gray-ramp observations",
        gray_count * gray_observations,
    );
    let transform = looklab_color::RgbSpace::Rec2020.to_xyz_matrix();
    let inverse = transform
        .inverse()
        .ok_or("singular synthetic camera matrix")?;
    for (image_index, image) in data.images.iter_mut().enumerate() {
        image.transform = transform;
        data.transforms[image_index] = image.transform;
        let mut inputs = Vec::with_capacity(samples_per_image);
        let mut outputs = Vec::with_capacity(samples_per_image);
        let mut points = Vec::with_capacity(samples_per_image);
        for (probe_index, observation, probe) in covered.iter().flat_map(|&(probe_index, probe)| {
            let observations = if probe.group == ProbeGroup::GrayRamp {
                gray_observations
            } else {
                1
            };
            (0..observations).map(move |observation| (probe_index, observation, probe))
        }) {
            let phase =
                (seed as usize + image_index * 37 + probe_index * 13 + observation * 101) as f64;
            let xyz = probe.xyz.scale(libm::exp2(0.08 * libm::sin(phase)));
            let rgb = inverse.mul_vec(xyz.to_array());
            inputs.push(rgb.map(|channel| channel as f32));
            let output = looklab_fit::post_clip(
                &data.truth,
                xyz,
                looklab_color::RgbSpace::Srgb,
                data.clip_k,
            );
            let noisy = Ucs::new(
                output.j + 0.1 * libm::sin(phase * 1.7),
                output.a + 0.1 * libm::sin(phase * 2.3),
                output.b + 0.1 * libm::cos(phase * 3.1),
            );
            let output_xyz = reference_model().jmh_to_xyz(ucs_to_jmh(noisy, UcsSpace::Ucs));
            outputs.push(
                looklab_color::RgbSpace::Srgb
                    .xyz_to_rgb_linear(output_xyz)
                    .to_array()
                    .map(|v| v as f32),
            );
            points.push(xyz);
        }
        let n = inputs.len();
        let mut header = image.samples.header().clone();
        header.n = n as u32;
        image.samples = SampleSet::new(
            header,
            inputs,
            outputs,
            vec![1.0; n],
            vec![SampleFlags::NONE; n],
        )?;
        data.points[image_index] = points;
    }
    Ok(data)
}

fn uncertainty(result: &looklab_fit::FitResult) -> Result<UncertaintyData, Box<dyn Error>> {
    let probes = result.probes.probes.len();
    if probes == 0 {
        return Ok(UncertaintyData::default());
    }
    let mut covariance = Vec::with_capacity(probes * 9);
    let mut covariance_post = Vec::with_capacity(probes * 9);
    let mut bias = Vec::with_capacity(probes);
    let mut sigma = Vec::with_capacity(probes);
    let mut sigma_post = Vec::with_capacity(probes);
    let mut reliable = Vec::with_capacity(probes);
    let mut support = Vec::with_capacity(probes);
    let mut density = Vec::with_capacity(probes);
    for probe in &result.probes.probes {
        for row in &probe.covariance {
            covariance.extend_from_slice(row);
        }
        for row in &probe.post_covariance {
            covariance_post.extend_from_slice(row);
        }
        bias.push(probe.bias_de);
        sigma.push(probe.sigma_de);
        sigma_post.push(probe.post_sigma_de);
        reliable.push(if probe.pre_clip_reliable { 1.0 } else { 0.0 });
        support.push(if probe.supported { 1.0 } else { 0.0 });
        density.push(probe.density);
    }
    Ok(UncertaintyData {
        probe_cov: Some(ProbeCovariance::new(probes, covariance)?),
        probe_cov_post: Some(ProbeCovariance::new(probes, covariance_post)?),
        probe_bias: Some(ProbeScalars::new(bias)?),
        probe_sigma: Some(ProbeScalars::new(sigma)?),
        probe_sigma_post: Some(ProbeScalars::new(sigma_post)?),
        probe_pre_reliable: Some(ProbeScalars::new(reliable)?),
        support: Some(ProbeScalars::new(support)?),
        density: Some(Density::new(density)?),
    })
}

fn provenance(
    data: &SyntheticDataset,
    result: &looklab_fit::FitResult,
) -> Result<Provenance, Box<dyn Error>> {
    let prep_versions = data
        .images
        .iter()
        .map(|image| image.samples.header().prep_version.get())
        .collect::<Vec<_>>();
    Ok(Provenance {
        bodies: vec![BodyProvenance {
            body: "fujifilm/x-t5".to_owned(),
            epoch_map_version: 4,
            epochs: vec![1],
            input_transform_version: 7,
            realization_params_version: 3,
            control_response_version: 1,
            clip_k: result.realization.clip_k,
            images: data.images.len() as u32,
            credentials: 1,
        }],
        root: RootProvenance {
            admissions: Vec::new(),
            anchor_version: "1.0.0".to_owned(),
        },
        community_snapshot_version: 1,
        prep_versions,
        created: CREATED.to_owned(),
        software: "looklab-publish site_data".to_owned(),
    })
}

fn provenance_summary(data: &SyntheticDataset) -> ProvenanceSummary {
    ProvenanceSummary {
        bodies: vec![BodyUse {
            body: "fujifilm/x-t5".to_owned(),
            epochs: vec![1],
            images: data.images.len() as u32,
            credentials: 1,
        }],
        root_admissions: Vec::new(),
        anchor_version: "1.0.0".to_owned(),
        community_snapshot_version: 1,
        prep_versions: vec![1],
        created: CREATED.to_owned(),
        software: "looklab-publish site_data".to_owned(),
    }
}

fn gates() -> Result<GateSummary, Box<dyn Error>> {
    Ok(GateSummary {
        checks: vec![
            GateCheck {
                name: "identity tolerance".to_owned(),
                passed: true,
                detail: Some("chromatic probes within 0.05 J'".to_owned()),
            },
            GateCheck {
                name: "tone monotone".to_owned(),
                passed: true,
                detail: None,
            },
            GateCheck {
                name: "held-out prediction".to_owned(),
                passed: true,
                detail: Some("median ΔE 0.41".to_owned()),
            },
            GateCheck {
                name: "artifact manifest".to_owned(),
                passed: true,
                detail: Some("every artifact listed by SHA-256".to_owned()),
            },
        ],
    })
}

#[derive(serde::Deserialize)]
struct FixtureSources {
    fixture: Vec<FixtureSource>,
}

#[derive(serde::Deserialize)]
struct FixtureSource {
    file: String,
    make: String,
    model: String,
    source_url: String,
    license: String,
    sha256: ArtifactHash,
    size: u64,
}

impl FixtureSource {
    fn credit(&self) -> String {
        let normalize = |value: &str| {
            let mut normalized = String::with_capacity(value.len());
            for word in value.split_whitespace() {
                if !normalized.is_empty() {
                    normalized.push(' ');
                }
                normalized.push_str(word);
            }
            normalized
        };
        let make = normalize(&self.make);
        let model = normalize(&self.model);
        let camera = if model
            .get(..make.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(&make))
        {
            model
        } else {
            format!("{make} {model}")
        };
        format!("{camera} · raw.pixls.us · CC0")
    }
}

fn canonical_scenes() -> Result<Vec<Scene>, Box<dyn Error>> {
    use looklab_raw::{RawDecoder, RawlerDecoder};
    let sources: FixtureSources = toml::from_str(include_str!("../../../fixtures/manifest.toml"))?;
    let mut scenes = Vec::new();
    if !std::env::args().any(|arg| arg == "--charts-only") {
        for (id, title) in [
            ("1955", "Riverside light"),
            ("3115", "Coastal dunes"),
            ("1824", "Autumn canopy"),
            ("1797", "Blue boat on a green waterway"),
            ("3094", "Frog among lily pads"),
            ("1603", "Sunlit brickwork"),
        ] {
            let path = format!("fixtures/cache/scene-{id}.dng");
            let bytes = fs::read(&path).map_err(|e| format!("{path}: {e}. Run tools/fetch-fixtures.sh, or pass --charts-only to omit photos explicitly."))?;
            let filename = format!("scene-{id}.dng");
            let source = sources
                .fixture
                .iter()
                .find(|entry| entry.file == filename)
                .ok_or_else(|| format!("{filename} is missing from fixtures/manifest.toml"))?;
            if source.sha256 != ArtifactHash::of(&bytes) || source.size != bytes.len() as u64 {
                return Err(format!(
                    "{path}: source checksum mismatch. Run tools/fetch-fixtures.sh."
                )
                .into());
            }
            let raw = RawlerDecoder.decode(&bytes)?;
            let profile = looklab_body::CameraProfile::from_raw(
                raw.color_matrices
                    .as_ref()
                    .ok_or("RAW has no color matrix")?,
            )?;
            let matrix = looklab_body::camera_to_xyz_at_wb(
                profile.spec(),
                raw.as_shot_wb.map(|v| v as f32),
            )?;
            let mut xyz: Vec<[f32; 3]> = raw
                .apply_wb()
                .as_chunks::<3>()
                .0
                .iter()
                .map(|rgb| {
                    matrix
                        .mul_vec([rgb[0] as f64, rgb[1] as f64, rgb[2] as f64])
                        .map(|v| v as f32)
                })
                .collect();
            let mut luminance: Vec<f32> = xyz
                .iter()
                .map(|p| p[1])
                .filter(|y| y.is_finite() && *y > 0.00001)
                .collect();
            if luminance.is_empty() {
                return Err(format!("{path}: no usable luminance").into());
            }
            let middle = luminance.len() / 2;
            luminance.select_nth_unstable_by(middle, f32::total_cmp);
            let exposure = 0.18 / luminance[middle];
            for pixel in &mut xyz {
                for value in pixel {
                    *value *= exposure;
                }
            }
            let mut scene = Scene::new(
                SceneName::new(format!("photo-{id}"))?,
                raw.width as u32,
                raw.height as u32,
                xyz,
            )?
            .downscale(1000);
            while scene.to_png()?.len() > 2_500_000 {
                scene = scene.downscale(scene.width.max(scene.height) * 9 / 10);
            }
            scene.kind = looklab_publish::SceneKind::Photo;
            scene.title = title.to_owned();
            scene.credit = Some(looklab_publish::SceneCredit {
                text: source.credit(),
                source_url: source.source_url.clone(),
                license: source.license.clone(),
                source_sha256: source.sha256.clone(),
            });
            println!(
                "photo {id}: {}x{}, orientation {:?}, {} PNG bytes",
                scene.width,
                scene.height,
                raw.orientation,
                scene.to_png()?.len()
            );
            scenes.push(scene);
        }
    }
    scenes.extend([gradient_scene(), hue_wheel_scene(), patches_scene()]);
    Ok(scenes)
}

fn gradient_scene() -> Scene {
    let colors = [
        [0.82, 0.18, 0.16],
        [0.88, 0.55, 0.18],
        [0.86, 0.82, 0.24],
        [0.28, 0.62, 0.30],
        [0.22, 0.44, 0.78],
        [0.48, 0.28, 0.66],
    ];
    let mut xyz = Vec::with_capacity((SIZE * SIZE) as usize);
    for y in 0..SIZE {
        let v = y as f64 / (SIZE - 1) as f64;
        for x in 0..SIZE {
            let u = x as f64 / (SIZE - 1) as f64;
            let encoded = if u < 0.5 {
                let level = 0.002 + 1.0 * (u / 0.5);
                [level, level, level]
            } else {
                let band =
                    (((u - 0.5) * colors.len() as f64).floor() as usize).min(colors.len() - 1);
                let shade = 0.10 + 0.90 * (1.0 - v);
                [
                    colors[band][0] * shade,
                    colors[band][1] * shade,
                    colors[band][2] * shade,
                ]
            };
            xyz.push(encoded_xyz(encoded));
        }
    }
    Scene::new(
        SceneName::new("gradient").expect("scene name"),
        SIZE,
        SIZE,
        xyz,
    )
    .expect("scene")
}

fn hue_wheel_scene() -> Scene {
    let mut xyz = Vec::with_capacity((SIZE * SIZE) as usize);
    let center = (SIZE - 1) as f64 / 2.0;
    let radius = center;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f64 - center;
            let dy = y as f64 - center;
            let distance = (dx * dx + dy * dy).sqrt() / radius;
            let angle = libm::atan2(dy, dx);
            let hue = (angle / (2.0 * core::f64::consts::PI)).rem_euclid(1.0);
            let saturation = distance.clamp(0.0, 1.0);
            let value = 1.0 - 0.55 * distance.clamp(0.0, 1.0);
            let encoded = hsv(hue, saturation, value);
            xyz.push(encoded_xyz(encoded));
        }
    }
    Scene::new(
        SceneName::new("hue-wheel").expect("scene name"),
        SIZE,
        SIZE,
        xyz,
    )
    .expect("scene")
}

fn patches_scene() -> Scene {
    let patches = [
        ([0.91, 0.76, 0.66], "light skin"),
        ([0.55, 0.36, 0.26], "deep skin"),
        ([0.22, 0.42, 0.16], "foliage"),
        ([0.44, 0.66, 0.92], "sky"),
        ([0.18, 0.18, 0.18], "neutral 18%"),
        ([0.92, 0.92, 0.92], "white"),
        ([0.04, 0.04, 0.04], "black"),
        ([0.78, 0.12, 0.10], "red"),
        ([0.16, 0.60, 0.22], "green"),
        ([0.12, 0.32, 0.78], "blue"),
        ([0.96, 0.72, 0.28], "highlight"),
        ([0.10, 0.09, 0.12], "shadow"),
    ];
    let cols = 4_u32;
    let rows = 3_u32;
    let mut xyz = Vec::with_capacity((SIZE * SIZE) as usize);
    for y in 0..SIZE {
        let row = (y * rows / SIZE).min(rows - 1) as usize;
        let v = (y % (SIZE / rows)) as f64 / (SIZE / rows) as f64;
        for x in 0..SIZE {
            let col = (x * cols / SIZE).min(cols - 1) as usize;
            let patch = patches[(row * cols as usize + col).min(patches.len() - 1)].0;
            let shade = 0.72 + 0.28 * v;
            xyz.push(encoded_xyz([
                patch[0] * shade,
                patch[1] * shade,
                patch[2] * shade,
            ]));
        }
    }
    Scene::new(
        SceneName::new("patches").expect("scene name"),
        SIZE,
        SIZE,
        xyz,
    )
    .expect("scene")
}

fn encoded_xyz(encoded: [f64; 3]) -> [f32; 3] {
    let xyz = looklab_color::RgbSpace::Srgb
        .rgb_encoded_to_xyz(looklab_color::Rgb::new(encoded[0], encoded[1], encoded[2]));
    [xyz.x as f32, xyz.y as f32, xyz.z as f32]
}

fn hsv(h: f64, s: f64, v: f64) -> [f64; 3] {
    let h6 = h * 6.0;
    let i = h6.floor();
    let f = h6 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match (i as i64).rem_euclid(6) {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

trait SecondBody {
    fn into_second_body(self) -> Option<BodyPage>;
}

impl SecondBody for BodyPage {
    fn into_second_body(mut self) -> Option<BodyPage> {
        let id = looklab_schema::BodyId::new("fujifilm/x-m1").ok()?;
        self.schema = self
            .schema
            .replacen("body: fujifilm/x-t5", "body: fujifilm/x-m1", 1);
        self.id = id;
        self.make = "Fujifilm".to_owned();
        self.model = "X-M1".to_owned();
        self.exif_aliases = vec!["X-M1".to_owned()];
        self.status = looklab_publish::BodyStatus::Active;
        Some(self)
    }
}
