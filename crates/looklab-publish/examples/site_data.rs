// SPDX-License-Identifier: AGPL-3.0-or-later
//! Generates sample Look fits, camera match bundles, CC0 photo scenes, test charts, bodies and an audit log.
//! The first argument selects the output directory; `--charts-only` omits photographs.
#![allow(clippy::disallowed_methods)]

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use looklab_export::card::{CardMeta, CardOptions, write_card};
use looklab_export::{LutLookOptions, import_lut_look, lut_data_from_cube, read_cube};
use looklab_fit::synth::{SynthConfig, SyntheticDataset, dataset};
use looklab_fit::{FitOptions, fit};
use looklab_match::{ControlCovariance, synth};
use looklab_model::file::{
    BaseReference, BaseStatus, BodyProvenance, DeclaredEncoding, DynamicRange, LicenseRecord,
    LookIdentity, LookKind, LookVersion, LutDefinition, LutEmulation, LutRecord, OutputEncoding,
    Provenance, Renderer, RootProvenance, Slug, WbPolicy,
};
use looklab_model::realization::{ControlResponseModel, encode_crm};
use looklab_model::{
    Density, Look, LookDocument, ProbeCovariance, ProbeScalars, Tone, UncertaintyData,
};
use looklab_publish::{
    ArtifactBlob, ArtifactHash, ArtifactName, AuditEntry, AuditEvent, AuditLog, BodyExtras,
    BodyPage, BodyUse, CARD_ARTIFACT, ConformanceEntry, CredentialRef, CredentialTier,
    DOCUMENT_ARTIFACT, Decision, ExcludedSim, GateCheck, GateSummary, InputTransformPage,
    LicensePage, LookListing, LookPage, LookRef, LookStatus, LookVersionPage, MatchBundle,
    MatchSim, PREVIEW_LUT_ARTIFACT, PreparationPage, ProposalRef, ProvenanceSummary,
    RealizationNote, Role, SYNTHETIC_SAMPLE_GATE, Scene, SimExclusion, SiteData, SitePath,
    Timestamp, TransformSource, VersionNotes, VersionRef, WeakSpot, bake_lut, bake_preview_lut,
    body_artifact, bundle_artifact, sim_artifacts, transform_artifact, write_conformance_pack,
    write_site,
};
use looklab_report::compare::{ComparisonInput, FunctionName, compare};
use looklab_schema::{BodyId, SimKey, validate_body, validate_control_schema};

const SIZE: u32 = 1414;
const CREATED: &str = "2026-11-04T00:00:00Z";
const DONOR_ATTRIBUTION: &str = "mistycycles (Discord), donated 2026-11-05";
const DONOR_LICENSE: &str = "CC0-1.0";

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
    bodies.push(fixture_body()?);

    let mut looks = synthetic_looks(&bodies)?;
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
    looks.extend(community_looks()?);
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
        community_snapshot_version: 0,
        bodies,
        manuals: Vec::new(),
        interfaces: Vec::new(),
        input_transforms: Vec::new(),
        looks,
        audit_log: AuditLog::new(audit),
        scenes: canonical_scenes()?,
        reports: Vec::new(),
        eligibility: Vec::new(),
        current: Vec::new(),
        bundles: Vec::new(),
        reference_preview: Some(ArtifactBlob::new(
            ArtifactName::new("neutral.lut33")?,
            bake_lut(&Look::colorimetric(), |xyz| xyz),
        )),
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
    publish_conformance_archive(&mut sink)?;
    println!(
        "wrote {}/data ({} bytes of index)",
        out.display(),
        fs::read(out.join("data/index.json"))
            .map(|bytes| bytes.len())
            .unwrap_or(0)
    );
    Ok(())
}

fn publish_conformance_archive(
    sink: &mut impl looklab_publish::SiteSink,
) -> Result<(), Box<dyn Error>> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/card/conformance-v1");
    let mut entries = Vec::new();
    collect_conformance_entries(&source, &source, &mut entries)?;
    write_conformance_pack(sink, &entries)?;
    Ok(())
}

fn collect_conformance_entries(
    root: &Path,
    directory: &Path,
    entries: &mut Vec<ConformanceEntry>,
) -> Result<(), Box<dyn Error>> {
    let mut paths = fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    for path in paths {
        let kind = fs::symlink_metadata(&path)?.file_type();
        if kind.is_dir() {
            collect_conformance_entries(root, &path, entries)?;
        } else if kind.is_file() {
            let relative = path.strip_prefix(root)?;
            let name = relative
                .to_str()
                .ok_or("conformance file name is not UTF-8")?;
            entries.push(ConformanceEntry {
                path: SitePath::new(name)?,
                bytes: fs::read(path)?,
            });
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
        preparation: PreparationPage::ready(),
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
        bundles: Vec::new(),
    };
    Ok(BodyPage::from_schema(&body, &schema, &extras)?)
}

fn fixture_body() -> Result<BodyPage, Box<dyn Error>> {
    let entity = include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml")
        .replace("id: fujifilm/x-t5", "id: fujifilm/x-s10")
        .replace("model: X-T5", "model: X-S10")
        .replace("exif_aliases: [X-T5]", "exif_aliases: [X-S10]");
    let controls = include_str!("../../../data/bodies/fujifilm-x-t5.yaml")
        .replace("body: fujifilm/x-t5", "body: fujifilm/x-s10");
    let body = validate_body(&entity).map_err(|errors| format!("fixture body: {errors:?}"))?;
    let schema = validate_control_schema(&controls)
        .map_err(|errors| format!("fixture schema: {errors:?}"))?;
    let extras = BodyExtras {
        preparation: PreparationPage::ready(),
        input_transform: None,
        realization_notes: vec![RealizationNote {
            epoch: None,
            text: "Sample data for the committed X-S10 RAW fixture. Camera entity and menu mappings are copied from the X-T5.".to_owned(),
        }],
        bundles: Vec::new(),
    };
    Ok(BodyPage::from_schema(&body, &schema, &extras)?)
}

fn community_looks() -> Result<Vec<LookPage>, Box<dyn Error>> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/luts");
    Ok(vec![
        community_look(
            &dir,
            CommunityLookSpec {
                file: "Ricoh_GRIV_Cinema_Green_SRGB.cube",
                slug: "ricoh-griv-cinema-green",
                name: "Ricoh GR IV Cinema Green",
                short_name: "Cinema Green",
                description: "Cinema Green from the Ricoh GR IV, the donor's original sRGB grade. The graded image is not stated, so the preview uses the neutral baseline and the Look is not matchable.",
                encoding: x_e3_base(None, DynamicRange::Dr200)?,
                emulation: Some(LutEmulation::CameraRendering {
                    maker: "ricoh".to_owned(),
                    rendering: "Cinema (Green)".to_owned(),
                }),
                provenance_note: "AI-tool approximation of Ricoh GR stock Cinema (Green) image control, sRGB variant; not graded by a person. Donor revision v3 lowers the brightest near-neutral tones by 1/3 stop.",
                base_note: Some("base incomplete: film simulation not declared"),
            },
        )?,
        community_look(
            &dir,
            CommunityLookSpec {
                file: "CineStill_800T_1600T_sRGB_V4.cube",
                slug: "cinestill-800t-pushed-1600",
                name: "CineStill 800T pushed to 1600",
                short_name: "CineStill 800T 1600",
                description: "Parametric approximation of CineStill 800T pushed one stop, built from published descriptions rather than measured scans. The intended base is Fujifilm Classic Negative at DR400, declared by the donor but not applied (their X-E3 lacks the sim); the preview uses the neutral baseline and the Look is not matchable until the base Look is published.",
                encoding: family_base("Classic Negative", DynamicRange::Dr400),
                emulation: Some(LutEmulation::FilmStock {
                    name: "CineStill 800T".to_owned(),
                }),
                provenance_note: "Intended base declared by the donor; the LUT was authored for this base and not applied on it (donor body X-E3 lacks the sim).",
                base_note: Some(
                    "base identified, unmeasured: fujifilm / Classic Negative at DR400",
                ),
            },
        )?,
        community_look(
            &dir,
            CommunityLookSpec {
                file: "Leica_Classic_Gold_200.cube",
                slug: "leica-classic-gold-200",
                name: "Leica Classic Gold 200",
                short_name: "Gold 200",
                description: "Unofficial donor-defined Classic Gold 200 grade, not an official Leica rendering or a confirmed emulation of one. The theoretical intended base is Fujifilm Eterna at DR200, declared by the donor but not applied (their X-E3 lacks the sim). The preview uses the neutral baseline and the Look is not matchable until the intended base is measured.",
                encoding: family_base("Eterna", DynamicRange::Dr200),
                emulation: None,
                provenance_note: "Unofficial donor-defined grade with no official Leica equivalent. The donor declared Eterna DR200 as the theoretical intended base but did not apply the LUT on it; their X-E3 lacks Eterna.",
                base_note: Some("base identified, unmeasured: fujifilm / Eterna at DR200"),
            },
        )?,
    ])
}

fn family_base(rendering: &str, dr: DynamicRange) -> DeclaredEncoding {
    DeclaredEncoding::DisplayReferred {
        base: BaseReference::FamilyRendering {
            maker: "fujifilm".to_owned(),
            rendering: rendering.to_owned(),
            dynamic_range: Some(dr),
            notes: "Donor body: fujifilm/x-e3. The LUT was authored for this intended base and not applied on it."
                .to_owned(),
        },
        status: BaseStatus::BaseIdentifiedUnmeasured,
    }
}

fn x_e3_base(sim: Option<&str>, dr: DynamicRange) -> Result<DeclaredEncoding, Box<dyn Error>> {
    let reference = BaseReference::BodyRealization {
        body: BodyId::new("fujifilm/x-e3")?,
        renderer: Renderer::XRawStudio,
        film_simulation: sim.map(SimKey::new).transpose()?,
        dynamic_range: Some(dr),
        notes: "X-RAW Studio (camera engine); other settings at camera defaults as far as known."
            .to_owned(),
    };
    let schema = validate_control_schema(include_str!("../../../data/bodies/fujifilm-x-e3.yaml"))
        .map_err(|errors| format!("{errors:?}"))?;
    let status = reference.resolve_status(Some(&schema));
    Ok(DeclaredEncoding::DisplayReferred {
        base: reference,
        status,
    })
}

struct CommunityLookSpec<'a> {
    file: &'a str,
    slug: &'a str,
    name: &'a str,
    short_name: &'a str,
    description: &'a str,
    encoding: DeclaredEncoding,
    emulation: Option<LutEmulation>,
    provenance_note: &'a str,
    base_note: Option<&'a str>,
}

fn community_look(dir: &Path, spec: CommunityLookSpec<'_>) -> Result<LookPage, Box<dyn Error>> {
    let CommunityLookSpec {
        file,
        slug,
        name,
        short_name,
        description,
        encoding,
        emulation,
        provenance_note,
        base_note,
    } = spec;
    let text = fs::read_to_string(dir.join(file))?;
    let cube = read_cube(&text)?;
    let lut = lut_data_from_cube(&cube)?;
    let definition = LutDefinition {
        name: name.to_owned(),
        license: LicenseRecord {
            own_work: false,
            source_license: Some(DONOR_LICENSE.to_owned()),
            source_url: None,
            attribution: DONOR_ATTRIBUTION.to_owned(),
        },
        encoding,
        declared_base: None,
        output: OutputEncoding::Srgb,
        lut: LutRecord {
            size: lut.size,
            domain_min: lut.domain_min,
            domain_max: lut.domain_max,
            values_ref: looklab_model::BinRef::new("bin:lut")?,
        },
        emulates: emulation,
        superseded_by: None,
        notes: vec![
            "Imported from a community-donated LUT.".to_owned(),
            provenance_note.to_owned(),
        ],
    };
    let identity = LookIdentity::new(slug, "1.0.0", LookKind::LutDefined, WbPolicy::Adaptive)?;
    let imported = import_lut_look(identity, definition, &cube, None, LutLookOptions::default())?;
    let mut document = imported.document;
    document.file.provenance.created = CREATED.to_owned();
    document.file.provenance.software = "looklab-publish site_data".to_owned();
    let look_bytes = document.encode()?;
    let preview = bake_preview_lut(&document);
    let mut artifacts = vec![
        ArtifactBlob::new(ArtifactName::new(DOCUMENT_ARTIFACT)?, look_bytes),
        ArtifactBlob::new(ArtifactName::new(PREVIEW_LUT_ARTIFACT)?, preview),
    ];
    if document.is_matchable() {
        let card = write_card(
            &document,
            &CardMeta::new(name, short_name)
                .with_description(description)
                .with_license(DONOR_LICENSE),
            &CardOptions::default(),
        )?;
        artifacts.push(ArtifactBlob::new(ArtifactName::new(CARD_ARTIFACT)?, card));
    }
    let p95 = imported.approximation.p95;
    let approximation_ok = looklab_export::approximation_publishable(&imported.approximation);
    let mut checks = vec![
        GateCheck {
            name: "license declaration".to_owned(),
            passed: true,
            detail: Some(format!("{DONOR_LICENSE}; {DONOR_ATTRIBUTION}")),
        },
        GateCheck {
            name: "approximation".to_owned(),
            passed: approximation_ok,
            detail: Some(format!(
                "p95 {:.3} ΔE on {} probes",
                p95, imported.approximation.probes
            )),
        },
    ];
    if document.is_matchable() {
        checks.push(GateCheck {
            name: "declared encoding".to_owned(),
            passed: true,
            detail: Some("matches the imported LUT".to_owned()),
        });
    } else {
        checks.push(GateCheck {
            name: "base".to_owned(),
            passed: false,
            detail: Some(base_note.unwrap_or("base unknown").to_owned()),
        });
    }
    let status = if document.is_matchable() && approximation_ok {
        LookStatus::Published
    } else {
        LookStatus::Provisional
    };
    let mut changelog = vec![format!(
        "Imported from {file} ({DONOR_LICENSE}, {DONOR_ATTRIBUTION})."
    )];
    if let Some(note) = base_note {
        changelog.push(note.to_owned());
    }
    let notice = if document.is_matchable() {
        None
    } else {
        Some(format!(
            "Not matchable and no card file: {}. The preview is approximate, not a target.",
            base_note.unwrap_or("base unknown")
        ))
    };
    let mut version = LookVersionPage::new(
        Slug::new(slug)?,
        LookVersion::new("1.0.0")?,
        status,
        GateSummary { checks },
        community_provenance_summary(),
        artifacts,
        VersionNotes {
            changelog,
            weak_spots: Vec::new(),
            notice,
        },
    )?;
    version.matchable = document.is_matchable();
    Ok(LookPage {
        slug: Slug::new(slug)?,
        name: name.to_owned(),
        kind: LookKind::LutDefined,
        wb_policy: WbPolicy::Adaptive,
        description: description.to_owned(),
        aliases: Vec::new(),
        license: Some(LicensePage {
            declaration: format!("Licensed for redistribution under {DONOR_LICENSE}"),
            attribution: DONOR_ATTRIBUTION.to_owned(),
        }),
        listing: LookListing::Listed,
        superseded_by: None,
        head: None,
        selected: None,
        versions: vec![version],
    })
}

fn community_provenance_summary() -> ProvenanceSummary {
    ProvenanceSummary {
        bodies: Vec::new(),
        root_admissions: Vec::new(),
        anchor_version: String::new(),
        community_snapshot_version: 0,
        prep_versions: Vec::new(),
        created: CREATED.to_owned(),
        software: "looklab-publish site_data".to_owned(),
    }
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
            Some("This version was produced by a revert to an earlier fit; the rejected contributions are listed in the audit log.".to_owned()),
        ),
    ];
    let mut looks = Vec::new();
    for (seed, slug, name, description, character, region, note, notice) in specs {
        let data = covered_dataset(seed, character, region)?;
        let result = fit(
            &data.images,
            &FitOptions {
                seed,
                max_samples_per_image: 2_000,
                hessian_samples: 8_000,
                // Leave room for the full `max_sweeps`; the convergence check is the gate.
                sweep_budget: 8_000_000.0,
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
        if !result.diagnostics.converged {
            return Err(format!(
                "{slug}: fit did not converge after {} evaluations",
                result.diagnostics.evaluations
            )
            .into());
        }
        report_recovery(slug, &data, &result, region);
        let probes = &result.probes.probes;
        let supported = probes.iter().filter(|probe| probe.supported).count();
        println!(
            "{slug}: converged {} in {} evaluations; supported {}/{}; sigma {}, density {}, clusters {}, bias {}",
            result.diagnostics.converged,
            result.diagnostics.evaluations,
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
            name: SYNTHETIC_SAMPLE_GATE.into(),
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
            listing: LookListing::Listed,
            superseded_by: None,
            head: None,
            selected: None,
            versions: vec![version],
        });
    }
    Ok(looks)
}

fn sample_match_artifacts(
    target: &LookDocument,
    bodies: &[BodyPage],
) -> Result<Vec<ArtifactBlob>, Box<dyn Error>> {
    let classic = synth::body(9)?;
    let classic_key = SimKey::new(synth::SYNTH_SIM)?;
    let provia_key = SimKey::new("provia")?;
    let provia = synth::body_for_sim(10, &provia_key)?;
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
            let body = schema.body.clone();
            artifacts.push(ArtifactBlob::new(
                body_artifact(&body, epoch.id)?,
                camera.schema.as_bytes().to_vec(),
            ));
            artifacts.push(ArtifactBlob::new(
                transform_artifact(&body, epoch.id)?,
                transform.as_bytes().to_vec(),
            ));
            let mut sims = Vec::new();
            let mut excluded = Vec::new();
            for sim in &epoch.native_sims {
                let model = if sim.key == classic_key {
                    Some(&classic)
                } else if sim.key == provia_key {
                    Some(&provia)
                } else {
                    None
                };
                let Some(model) = model else {
                    excluded.push(ExcludedSim {
                        sim: sim.key.clone(),
                        reason: SimExclusion::NoPublishedLook,
                    });
                    continue;
                };
                let base = LookDocument::from_look(
                    target.file.look.clone(),
                    model.model.base_look.clone(),
                )?
                .encode()?;
                let crm = if model.model.crm.body == body && model.model.crm.epoch == epoch.id {
                    model.model.crm.clone()
                } else {
                    ControlResponseModel::zeroed(&schema, epoch.id, &sim.key, 1)?
                };
                let units = crm.parameter_ids();
                let count = units.len();
                let mut covariance = vec![0.0_f64; count * count];
                for index in 0..count {
                    covariance[index * count + index] = 1.0e-4;
                }
                let covariance = ControlCovariance {
                    parameter_ids: units,
                    covariance,
                }
                .to_json()?;
                let names = sim_artifacts(&body, epoch.id, &sim.key)?;
                artifacts.push(ArtifactBlob::new(names.base.clone(), base));
                artifacts.push(ArtifactBlob::new(names.crm.clone(), encode_crm(&crm)?));
                artifacts.push(ArtifactBlob::new(names.covariance.clone(), covariance));
                sims.push(MatchSim {
                    sim: sim.key.clone(),
                    base: names.base,
                    crm: names.crm,
                    covariance: Some(names.covariance),
                });
            }
            let bundle = MatchBundle::new(body.clone(), epoch.id, sims, excluded)?;
            artifacts.push(ArtifactBlob::new(
                bundle_artifact(&body, epoch.id)?,
                bundle.to_json()?,
            ));
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
    let mut supported: Vec<(f64, f64)> = Vec::new();
    let mut covered_index = 0usize;
    for (index, probe) in probes.probes.iter().enumerate() {
        if gap.excludes(probe.xyz) {
            continue;
        }
        if let Some(estimate) = result.probes.probes.get(index)
            && estimate.supported
        {
            supported.push((errors[covered_index], estimate.post_sigma_de));
        }
        covered_index += 1;
    }
    let inside = supported
        .iter()
        .filter(|(error, sigma)| error <= &(2.0 * sigma))
        .count();
    let mut ratios: Vec<f64> = supported
        .iter()
        .map(|(error, sigma)| error / sigma.max(1e-9))
        .collect();
    ratios.sort_by(f64::total_cmp);
    let ratio = ratios.get(ratios.len() / 2).copied().unwrap_or(0.0);
    println!(
        "{slug}: supported coverage {}/{} = {:.3}, median error/sigma {ratio:.3}",
        inside,
        supported.len(),
        inside as f64 / supported.len().max(1) as f64,
    );
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
        base: None,
        needs_rebase: false,
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

fn canonical_scenes() -> Result<Vec<Scene>, Box<dyn Error>> {
    let mut scenes = Vec::new();
    if !std::env::args().any(|arg| arg == "--charts-only") {
        scenes.extend(looklab_publish::photo::prepare_photo_scenes(
            include_str!("../../../fixtures/manifest.toml"),
            Path::new("fixtures/cache"),
        )?);
    }
    scenes.extend(looklab_publish::test_chart_scenes(SIZE)?);
    Ok(scenes)
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
