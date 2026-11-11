// SPDX-License-Identifier: AGPL-3.0-or-later
//! Throwaway unchanged-limit evaluation of complete-system native fitting.

#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

#[cfg(feature = "synth")]
mod bounded {
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Read};
    use std::process::{Command, ExitCode, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use sha2::{Digest, Sha256};

    use looklab_color::ucs::delta_e_ucs;
    use looklab_color::{Rgb, RgbSpace, Xyz};
    use looklab_fit::synth::{
        self, ControlSynthConfig, LookSelector, SynthConfig, controlled_dataset,
    };
    use looklab_fit::{
        ControlResponseModel, ControlValue, FitImage, FitOptions, FitResult, FitState,
        FitStateGeneration, Settings, fit_controlled_with_history_resumable,
        fit_with_history_resumable,
    };
    use looklab_model::output_encode_clip;
    use looklab_model::realization::realize;
    use looklab_model::solve::ucs_of_xyz;
    use looklab_schema::{ControlKey, EnumValue, SimKey, validate_control_schema};

    const SCHEMA: &str = include_str!("../../../data/bodies/fujifilm-x-t5.yaml");
    const FOLD_CLIP_K: f64 = 0.03;
    const STATIONARY_DECREMENT_NATS: f64 = 1e-3;
    static CAPTURE_FAILED: AtomicBool = AtomicBool::new(false);

    // ---- fixtures, copied from the tests they reproduce ----

    fn schema() -> looklab_schema::ControlSchema {
        validate_control_schema(SCHEMA).expect("body schema validates")
    }

    fn sim() -> SimKey {
        SimKey::new("classic_chrome").expect("sim key")
    }

    fn key(name: &str) -> ControlKey {
        ControlKey::new(name).expect("control key")
    }

    fn step(name: &str, value: i64, settings: &mut Settings) {
        settings.set(key(name), ControlValue::Step(value));
    }

    fn name(name: &str, value: &str, settings: &mut Settings) {
        settings.set(
            key(name),
            ControlValue::Enum(EnumValue::new(value).expect("enum value")),
        );
    }

    fn library_settings() -> Vec<Settings> {
        let mut all = Vec::new();
        for _ in 0..6 {
            all.push(Settings::new());
        }
        for (control, value) in [
            ("color", 2),
            ("color", -2),
            ("color", 4),
            ("color", -4),
            ("highlight_tone", 2),
            ("highlight_tone", -2),
            ("highlight_tone", 4),
            ("highlight_tone", -4),
            ("shadow_tone", 3),
            ("shadow_tone", -3),
            ("shadow_tone", 4),
            ("shadow_tone", -4),
            ("wb_shift_r", 4),
            ("wb_shift_r", 6),
            ("wb_shift_b", -4),
            ("wb_shift_b", -6),
        ] {
            let mut settings = Settings::new();
            step(control, value, &mut settings);
            all.push(settings);
        }
        for mode in ["dr200", "dr400"] {
            let mut settings = Settings::new();
            name("dynamic_range", mode, &mut settings);
            all.push(settings);
        }
        for effect in ["weak", "strong"] {
            let mut settings = Settings::new();
            name("color_chrome_effect", effect, &mut settings);
            all.push(settings);
            let mut settings = Settings::new();
            name("color_chrome_fx_blue", effect, &mut settings);
            all.push(settings);
        }
        all
    }

    fn group_settings() -> Vec<Settings> {
        let mut all = Vec::new();
        let mut settings = Settings::new();
        step("color", 3, &mut settings);
        step("highlight_tone", 4, &mut settings);
        all.push(settings);
        let mut settings = Settings::new();
        step("color", -3, &mut settings);
        step("shadow_tone", 4, &mut settings);
        step("wb_shift_r", 6, &mut settings);
        all.push(settings);
        let mut settings = Settings::new();
        name("dynamic_range", "dr400", &mut settings);
        name("color_chrome_effect", "strong", &mut settings);
        step("exposure_compensation", 3, &mut settings);
        all.push(settings);
        let mut settings = Settings::new();
        step("color", 4, &mut settings);
        name("color_chrome_fx_blue", "weak", &mut settings);
        step("wb_shift_b", -6, &mut settings);
        all.push(settings);
        all
    }

    fn controlled_config() -> ControlSynthConfig {
        let schema = schema();
        let mut settings = library_settings();
        settings.extend(group_settings());
        let mut groups = vec![None; library_settings().len()];
        groups.extend(vec![Some([7; 16]); group_settings().len()]);
        ControlSynthConfig {
            base: SynthConfig {
                seed: 0x2211,
                images: settings.len(),
                samples_per_image: 120,
                noise_ucs: 0.4,
                clip_k: 0.03,
                ..SynthConfig::default()
            },
            schema,
            epoch: 1,
            base_sim: sim(),
            settings,
            groups,
            response_scale: 0.08,
            truth_seed: 0x3311,
        }
    }

    fn controlled_options() -> FitOptions {
        FitOptions {
            seed: 0x5511,
            fit_residual: false,
            max_sweeps: 24,
            ..FitOptions::default()
        }
    }

    fn model_error_dataset() -> synth::SyntheticDataset {
        synth::dataset(&SynthConfig {
            images: 4,
            samples_per_image: 200,
            ..SynthConfig::default()
        })
    }

    fn model_error_options() -> FitOptions {
        FitOptions {
            max_samples_per_image: 200,
            hessian_samples: 500,
            max_sweeps: 64,
            sweep_budget: 2_000_000.0,
            ..FitOptions::default()
        }
    }

    // ---- public-API measurements ----

    fn percentiles(values: &mut [f64]) -> (f64, f64) {
        if values.is_empty() {
            return (f64::NAN, f64::NAN);
        }
        values.sort_by(f64::total_cmp);
        let median = values[values.len() / 2];
        let p95 = values[(values.len() * 95 / 100).min(values.len() - 1)];
        (median, p95)
    }

    /// Median exposure gauge over the fitted images, using the truth nuisance for the same image.
    fn gauge(result: &FitResult, nuisances: &[synth::Nuisance], indices: &[usize]) -> f64 {
        let mut shifts: Vec<f64> = result
            .images
            .iter()
            .enumerate()
            .map(|(position, image)| {
                let truth = indices
                    .get(position)
                    .and_then(|index| nuisances.get(*index))
                    .map_or(0.0, |nuisance| nuisance.exposure);
                image.exposure - truth
            })
            .collect();
        if shifts.is_empty() {
            return 0.0;
        }
        shifts.sort_by(f64::total_cmp);
        shifts[shifts.len() / 2]
    }

    /// Held-out ΔE between the fitted and true latent Look at noise-free scene colours.
    fn latent_percentiles(
        look: &looklab_fit::Look,
        truth: &looklab_fit::Look,
        points: &[Vec<Xyz>],
        gauge: f64,
    ) -> (f64, f64) {
        let shift = libm::pow(2.0, gauge);
        let mut errors: Vec<f64> = points
            .iter()
            .flatten()
            .map(|xyz| delta_e_ucs(look.eval(xyz.scale(shift)), truth.eval(*xyz)))
            .collect();
        percentiles(&mut errors)
    }

    fn prediction_error(
        look: &looklab_fit::Look,
        model: &ControlResponseModel,
        image: &FitImage,
        settings: &Settings,
        exposure: f64,
        wb: [f64; 2],
    ) -> Vec<f64> {
        let realization = realize(look, model, settings).expect("realizes");
        let space = match image.samples.header().output_space {
            looklab_samples::OutputSpace::Srgb => RgbSpace::Srgb,
            looklab_samples::OutputSpace::AdobeRgb => RgbSpace::AdobeRgb,
        };
        let scale = libm::pow(2.0, exposure + realization.exposure_ev);
        let gain = [
            realization.wb_gain[0] * libm::exp(wb[0]),
            realization.wb_gain[1],
            realization.wb_gain[2] * libm::exp(wb[1]),
        ];
        let mut errors = Vec::new();
        for (in_rgb, out_rgb) in image.samples.in_rgb().iter().zip(image.samples.out_rgb()) {
            let cam = [
                f64::from(in_rgb[0]) * gain[0] * scale,
                f64::from(in_rgb[1]) * gain[1] * scale,
                f64::from(in_rgb[2]) * gain[2] * scale,
            ];
            let xyz = Xyz::from_array(image.transform.mul_vec(cam));
            let predicted =
                output_encode_clip(realization.look.eval(xyz), space, realization.look.clip.k);
            let target =
                ucs_of_xyz(space.rgb_linear_to_xyz(Rgb::from_array(out_rgb.map(f64::from))));
            errors.push(delta_e_ucs(predicted, target));
        }
        errors
    }

    // ---- case results ----

    struct FixtureResult {
        name: &'static str,
        status: String,
        note: String,
        converged: bool,
        sweeps: Option<usize>,
        evaluations: Option<usize>,
        sample_evaluations: Option<u64>,
        final_cost: Option<f64>,
        hessian_samples: Option<usize>,
        operator_rows: Option<usize>,
        objective_samples: Option<usize>,
        latent_median: f64,
        latent_p95: f64,
        gauge: f64,
        quality_name: QualityKind,
        quality_median: f64,
        quality_threshold: f64,
        quality_p95: f64,
        quality_p95_threshold: f64,
        quality_pass: bool,
        evidence: Option<bool>,
        wall_s: f64,
    }

    struct QualityReport {
        name: QualityKind,
        median: f64,
        p95: f64,
        threshold: f64,
        p95_threshold: f64,
    }

    impl FixtureResult {
        fn error(name: &'static str, note: String, wall_s: f64) -> Self {
            Self {
                name,
                status: "error".to_string(),
                note,
                converged: false,
                sweeps: None,
                evaluations: None,
                sample_evaluations: None,
                final_cost: None,
                hessian_samples: None,
                operator_rows: None,
                objective_samples: None,
                latent_median: f64::NAN,
                latent_p95: f64::NAN,
                gauge: f64::NAN,
                quality_name: QualityKind::None,
                quality_median: f64::NAN,
                quality_threshold: f64::NAN,
                quality_p95: f64::NAN,
                quality_p95_threshold: f64::NAN,
                quality_pass: false,
                evidence: None,
                wall_s,
            }
        }
    }

    fn error_note(error: &looklab_fit::FitError) -> String {
        match error {
            looklab_fit::FitError::NoModelErrorEvidence => "NoModelErrorEvidence".to_string(),
            other => format!("{other}"),
        }
    }

    fn from_fit(
        name: &'static str,
        result: FitResult,
        latent: (f64, f64),
        gauge: f64,
        quality: QualityReport,
        wall_s: f64,
    ) -> FixtureResult {
        let quality_pass = match quality.name {
            QualityKind::None => true,
            QualityKind::HeldoutRerender => {
                quality.median.is_finite() && quality.median <= quality.threshold
            }
            QualityKind::ModelErrorEvidence => {
                quality.median.is_finite() && quality.median <= quality.threshold
            }
        };
        FixtureResult {
            name,
            status: "ok".to_string(),
            note: String::new(),
            converged: result.diagnostics.converged,
            sweeps: Some(result.diagnostics.sweeps),
            evaluations: Some(result.diagnostics.evaluations),
            sample_evaluations: Some(result.diagnostics.sample_evaluations.get()),
            final_cost: Some(result.diagnostics.final_cost),
            hessian_samples: Some(result.diagnostics.hessian_samples),
            operator_rows: Some(result.diagnostics.operator_rows),
            objective_samples: Some(result.diagnostics.objective_samples),
            latent_median: latent.0,
            latent_p95: latent.1,
            gauge,
            quality_name: quality.name,
            quality_median: quality.median,
            quality_threshold: quality.threshold,
            quality_p95: quality.p95,
            quality_p95_threshold: quality.p95_threshold,
            quality_pass,
            evidence: None,
            wall_s,
        }
    }

    // ---- child cases ----

    enum CapturedCase {
        ModelErrorFold0,
        ModelErrorFold1,
        ControlledRecovery,
    }

    impl CapturedCase {
        fn file_name(self) -> &'static str {
            match self {
                Self::ModelErrorFold0 => "model-error-fold-0.json",
                Self::ModelErrorFold1 => "model-error-fold-1.json",
                Self::ControlledRecovery => "controlled-recovery.json",
            }
        }
    }

    fn capture_checkpoint(case: CapturedCase, state: Option<&FitState>) {
        let Some(directory) = std::env::var_os("LOOKLAB_BOUNDED_CAPTURE_DIR") else {
            return;
        };
        let file = std::path::PathBuf::from(directory).join(case.file_name());
        let Some(state) = state else {
            CAPTURE_FAILED.store(true, Ordering::Relaxed);
            eprintln!(
                "bounded checkpoint capture failed: {}: state unavailable",
                file.display()
            );
            return;
        };
        let saved = serde_json::to_vec(state)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&file, bytes));
        match saved {
            Ok(()) => eprintln!("bounded checkpoint captured: {}", file.display()),
            Err(error) => {
                CAPTURE_FAILED.store(true, Ordering::Relaxed);
                eprintln!(
                    "bounded checkpoint capture failed: {}: {error}",
                    file.display()
                );
            }
        }
    }

    fn replay_frozen_native(path: &str) -> Result<(), String> {
        let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
        let mut state: FitState =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        if state.generation != FitStateGeneration::CURRENT {
            return Err(format!(
                "frozen objective generation {} differs from current {}",
                state.generation.0,
                FitStateGeneration::CURRENT.0
            ));
        }
        if state.stage != 4 {
            return Err(format!(
                "frozen state is at stage {}, not final refinement",
                state.stage
            ));
        }
        state.converged = false;
        let data = model_error_dataset();
        let kept = &data.images[..2];
        let mut options = model_error_options();
        options.run_gcv = false;
        options.max_sweeps = state.sweep.checked_add(1).ok_or("frozen sweep overflow")?;
        println!(
            "bounded-frozen-input stage={} sweep={} diagnostic_cap={} coordinates={} damping={:.17e}",
            state.stage,
            state.sweep,
            options.max_sweeps,
            state.theta.len(),
            state.damping
        );
        let started = Instant::now();
        let (result, _) = fit_with_history_resumable(kept, &options, None, Some(&state))
            .map_err(|error| error_note(&error))?;
        let wall_s = started.elapsed().as_secs_f64();
        let kept_indices = [0, 1];
        let shift = gauge(&result, &data.nuisances, &kept_indices);
        let latent = latent_percentiles(&result.look, &data.truth, &data.points, shift);
        print_child(&from_fit(
            "frozen-native-step",
            result,
            latent,
            shift,
            QualityReport {
                name: QualityKind::None,
                median: f64::NAN,
                p95: f64::NAN,
                threshold: f64::NAN,
                p95_threshold: f64::NAN,
            },
            wall_s,
        ));
        Ok(())
    }

    fn model_error_fold(fold: usize) -> FixtureResult {
        let data = model_error_dataset();
        let start = fold * 2;
        let kept_indices: Vec<usize> = (0..data.images.len())
            .filter(|index| !(start..start + 2).contains(index))
            .collect();
        let kept: Vec<FitImage> = kept_indices
            .iter()
            .map(|index| data.images[*index].clone())
            .collect();
        let started = Instant::now();
        match fit_with_history_resumable(&kept, &model_error_options(), None, None) {
            Ok((result, state)) => {
                let wall_s = started.elapsed().as_secs_f64();
                if !result.diagnostics.converged {
                    capture_checkpoint(
                        if fold == 0 {
                            CapturedCase::ModelErrorFold0
                        } else {
                            CapturedCase::ModelErrorFold1
                        },
                        state.as_ref(),
                    );
                }
                let shift = gauge(&result, &data.nuisances, &kept_indices);
                let latent = latent_percentiles(&result.look, &data.truth, &data.points, shift);
                let label = if fold == 0 {
                    "model-error-fold-0"
                } else {
                    "model-error-fold-1"
                };
                from_fit(
                    label,
                    result,
                    latent,
                    shift,
                    QualityReport {
                        name: QualityKind::None,
                        median: f64::NAN,
                        p95: f64::NAN,
                        threshold: f64::NAN,
                        p95_threshold: f64::NAN,
                    },
                    wall_s,
                )
            }
            Err(error) => FixtureResult::error(
                if fold == 0 {
                    "model-error-fold-0"
                } else {
                    "model-error-fold-1"
                },
                error_note(&error),
                started.elapsed().as_secs_f64(),
            ),
        }
    }

    fn model_error_fold_0() -> FixtureResult {
        model_error_fold(0)
    }

    fn model_error_fold_1() -> FixtureResult {
        model_error_fold(1)
    }

    fn model_error_call() -> FixtureResult {
        const NAME: &str = "model-error-call";
        let data = model_error_dataset();
        let started = Instant::now();
        match looklab_fit::estimate_model_error(
            &data.images,
            &[],
            None,
            &model_error_options(),
            FOLD_CLIP_K,
        ) {
            Ok(field) => {
                let evidence = field.held_out_mean_rms.iter().any(|value| *value > 0.0);
                let mut result =
                    FixtureResult::error(NAME, String::new(), started.elapsed().as_secs_f64());
                result.status = "ok".to_string();
                result.converged = evidence;
                result.quality_name = QualityKind::ModelErrorEvidence;
                result.quality_median = if evidence { 1.0 } else { 0.0 };
                result.quality_threshold = 1.0;
                result.quality_pass = evidence;
                result.evidence = Some(evidence);
                result
            }
            Err(error) => {
                FixtureResult::error(NAME, error_note(&error), started.elapsed().as_secs_f64())
            }
        }
    }

    fn controlled_recovery() -> FixtureResult {
        const NAME: &str = "controlled-recovery";
        let config = controlled_config();
        let library = library_settings();
        let dataset = controlled_dataset(&config);
        let train = &dataset.images[..library.len()];
        let started = Instant::now();
        match fit_controlled_with_history_resumable(
            train,
            &dataset.schema,
            1,
            &sim(),
            &controlled_options(),
            None,
            None,
        ) {
            Ok((result, state)) => {
                let wall_s = started.elapsed().as_secs_f64();
                if !result.diagnostics.converged {
                    capture_checkpoint(CapturedCase::ControlledRecovery, state.as_ref());
                }
                let model = result
                    .control_response
                    .as_ref()
                    .expect("control model")
                    .model
                    .clone();
                let mut errors = Vec::new();
                for (index, settings) in group_settings().iter().enumerate() {
                    let image = &dataset.images[library.len() + index];
                    errors.extend(prediction_error(
                        &result.look,
                        &model,
                        image,
                        settings,
                        image.exposure_hint,
                        [0.0, 0.0],
                    ));
                }
                let quality = percentiles(&mut errors);
                let base = synth::dataset_with(
                    &config.base,
                    &dataset.truth_look,
                    &LookSelector::Single(&dataset.truth_look),
                );
                let indices: Vec<usize> = (0..library.len()).collect();
                let shift = gauge(&result, &base.nuisances, &indices);
                let latent = latent_percentiles(
                    &result.look,
                    &base.truth,
                    &base.points[..library.len()],
                    shift,
                );
                from_fit(
                    NAME,
                    result,
                    latent,
                    shift,
                    QualityReport {
                        name: QualityKind::HeldoutRerender,
                        median: quality.0,
                        p95: quality.1,
                        threshold: 1.5,
                        p95_threshold: f64::NAN,
                    },
                    wall_s,
                )
            }
            Err(error) => {
                FixtureResult::error(NAME, error_note(&error), started.elapsed().as_secs_f64())
            }
        }
    }

    // ---- result printing ----

    fn fmt_float(value: Option<f64>) -> String {
        match value {
            Some(value) if value.is_finite() => format!("{value:.9e}"),
            Some(value) if value.is_nan() => "nan".to_string(),
            Some(value) if value.is_infinite() => "inf".to_string(),
            _ => "-".to_string(),
        }
    }

    fn fmt_plain(value: f64) -> String {
        if value.is_finite() {
            format!("{value:.9e}")
        } else if value.is_nan() {
            "nan".to_string()
        } else {
            "inf".to_string()
        }
    }

    fn fmt_count(value: Option<usize>) -> String {
        value.map_or_else(|| "-".to_string(), |value| value.to_string())
    }

    fn cert_kind_str(kind: CertKind) -> &'static str {
        match kind {
            CertKind::Lower => "lower",
            CertKind::Upper => "upper",
            CertKind::None => "none",
        }
    }

    fn cert_kind_opt_str(kind: Option<CertKind>) -> &'static str {
        kind.map_or("-", cert_kind_str)
    }

    fn verdict_str(verdict: Option<Verdict>) -> &'static str {
        match verdict {
            Some(Verdict::Stationary) => "stationary",
            Some(Verdict::Nonstationary) => "nonstationary",
            Some(Verdict::Unresolved) => "unresolved",
            Some(Verdict::Other) => "other",
            None => "-",
        }
    }

    fn print_child(result: &FixtureResult) {
        println!(
            "bounded-fit-result fixture={} status={} converged={} sweeps={} evaluations={} samples={} final_cost={} hessian={} operator_rows={} objective_samples={} latent_median={} latent_p95={} gauge={} quality={} quality_median={} quality_threshold={} quality_p95={} quality_p95_threshold={} quality_pass={} evidence={} wall_s={} note={}",
            result.name,
            result.status,
            u8::from(result.converged),
            fmt_count(result.sweeps),
            fmt_count(result.evaluations),
            result
                .sample_evaluations
                .map_or_else(|| "-".to_string(), |value| value.to_string()),
            fmt_float(result.final_cost),
            fmt_count(result.hessian_samples),
            fmt_count(result.operator_rows),
            fmt_count(result.objective_samples),
            fmt_plain(result.latent_median),
            fmt_plain(result.latent_p95),
            fmt_plain(result.gauge),
            result.quality_name.as_str(),
            fmt_plain(result.quality_median),
            fmt_plain(result.quality_threshold),
            fmt_plain(result.quality_p95),
            fmt_plain(result.quality_p95_threshold),
            u8::from(result.quality_pass),
            result
                .evidence
                .map_or_else(|| "-".to_string(), |value| u8::from(value).to_string()),
            result.wall_s,
            result.note.replace('\n', " "),
        );
    }

    // ---- diagnostic parsing ----

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum CertKind {
        Lower,
        Upper,
        None,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Verdict {
        Stationary,
        Nonstationary,
        Unresolved,
        Other,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Stationarity {
        CertifiedStationary,
        CertifiedNonstationary,
        Uncertified,
        UncertifiedIncomplete,
        Unresolved,
        NotApplicable,
    }

    impl Stationarity {
        fn as_str(self) -> &'static str {
            match self {
                Self::CertifiedStationary => "certified-stationary",
                Self::CertifiedNonstationary => "certified-nonstationary",
                Self::Uncertified => "uncertified",
                Self::UncertifiedIncomplete => "uncertified-incomplete",
                Self::Unresolved => "unresolved",
                Self::NotApplicable => "not-applicable",
            }
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum QualityKind {
        None,
        HeldoutRerender,
        ModelErrorEvidence,
    }

    impl QualityKind {
        fn as_str(self) -> &'static str {
            match self {
                Self::None => "none",
                Self::HeldoutRerender => "heldout-rerender",
                Self::ModelErrorEvidence => "model-error-evidence",
            }
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum ChildStatus {
        Ok,
        Error,
    }

    impl ChildStatus {
        fn as_str(self) -> &'static str {
            match self {
                Self::Ok => "ok",
                Self::Error => "error",
            }
        }
    }

    fn child_status(fields: &BTreeMap<String, String>) -> ChildStatus {
        match fields.get("status").map(String::as_str) {
            Some("ok") => ChildStatus::Ok,
            _ => ChildStatus::Error,
        }
    }

    #[derive(Clone)]
    struct Certificate {
        kind: CertKind,
        value: f64,
        lower: Option<f64>,
        complete: Option<bool>,
        residual_coords: Option<usize>,
        quadratic: Option<f64>,
        first: Option<f64>,
        residual_correction: Option<f64>,
        iterations: Option<usize>,
        relative: Option<f64>,
        stalled: Option<bool>,
        joint: Option<usize>,
        outer: Option<usize>,
        border: Option<usize>,
        verdict: Option<Verdict>,
        verdict_kind: Option<CertKind>,
        verdict_termination: Option<String>,
        verdict_lower: Option<f64>,
        verdict_upper: Option<f64>,
        verdict_residual: Option<f64>,
        verdict_iterations: Option<usize>,
        verdict_dim: Option<usize>,
        verdict_residual_coords: Option<usize>,
        verdict_complete: Option<bool>,
        returned: bool,
        sweep: usize,
        raw: String,
    }

    #[derive(Clone, Copy, Default, PartialEq, Eq)]
    enum LinearSolveStatus {
        Success,
        Failed,
        #[default]
        Unreported,
    }

    #[derive(Clone, Copy)]
    enum TrustTermination {
        Interior,
        Boundary,
        NegativeCurvature,
        Invalid,
    }

    #[derive(Clone, Copy)]
    enum StepSelection {
        TrustRegion,
        Cauchy,
        Box,
        Invalid,
    }

    #[derive(Clone)]
    struct TrustTrial {
        termination: TrustTermination,
        selected: StepSelection,
        proposal_residual: Option<f64>,
        radius: Option<f64>,
        scale: Option<f64>,
    }

    #[derive(Clone, Default)]
    struct Trial {
        trial_cost: Option<f64>,
        current: Option<f64>,
        ratio: Option<f64>,
        prediction: Option<f64>,
        residual: Option<f64>,
        tolerance: Option<f64>,
        iterations: Option<usize>,
        dim: Option<usize>,
        residual_coords: Option<usize>,
        damping: Option<f64>,
        step: Option<f64>,
        solve_status: LinearSolveStatus,
        trust: Option<TrustTrial>,
    }

    #[derive(Clone, Default)]
    struct CoupledResidual {
        dim: Option<usize>,
        residual_coords: Option<usize>,
    }

    impl Trial {
        fn decrease(&self) -> Option<f64> {
            match (self.ratio, self.prediction) {
                (Some(ratio), Some(prediction)) if ratio.is_finite() && prediction.is_finite() => {
                    Some(ratio * prediction)
                }
                _ => None,
            }
        }

        fn coupled(&self) -> CoupledResidual {
            CoupledResidual {
                dim: self.dim,
                residual_coords: self.residual_coords,
            }
        }
    }

    #[derive(Default)]
    struct Diag {
        fits: usize,
        samples: Option<usize>,
        max_stage: usize,
        stage_start_cost: Option<f64>,
        last_cost: Option<f64>,
        last_cost_sweep: Option<usize>,
        accepted: Vec<Trial>,
        last_accepted: Option<Trial>,
        last_accepted_sweep: Option<usize>,
        last_rejected: Option<Trial>,
        last_solve_residual: Option<f64>,
        last_solve_iterations: Option<usize>,
        last_solve_tolerance: Option<f64>,
        last_solve_stalled: Option<bool>,
        last_solve_failure: Option<String>,
        solve_failures: BTreeMap<String, usize>,
        last_joint_residual: Option<f64>,
        last_coupled: Option<CoupledResidual>,
        certificate: Option<Certificate>,
        certificate_failure: Option<String>,
        plateau: Option<f64>,
        termination: Option<String>,
        gn_fallback: usize,
        gn_failed: usize,
    }

    fn token_after<'a>(line: &'a str, key: &str) -> Option<&'a str> {
        let key = key.trim();
        line.rmatch_indices(key).find_map(|(at, _)| {
            let before = &line[..at];
            let after = &line[at + key.len()..];
            if before
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_whitespace())
                || !after.chars().next().is_some_and(char::is_whitespace)
            {
                return None;
            }
            after.split_whitespace().next()
        })
    }

    fn number_after(line: &str, key: &str) -> Option<f64> {
        token_after(line, key).and_then(|token| token.parse().ok())
    }

    fn number_or_none_after(line: &str, key: &str) -> Option<f64> {
        match token_after(line, key)? {
            "none" => None,
            token => token.parse().ok(),
        }
    }

    fn number_after_first(line: &str, key: &str) -> Option<f64> {
        let at = line.find(key)? + key.len();
        line[at..].split_whitespace().next()?.parse().ok()
    }

    fn bool_after(line: &str, key: &str) -> Option<bool> {
        match token_after(line, key)? {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        }
    }

    fn parse_verdict(token: Option<&str>) -> Option<Verdict> {
        Some(match token? {
            "stationary" => Verdict::Stationary,
            "nonstationary" => Verdict::Nonstationary,
            "unresolved" => Verdict::Unresolved,
            _ => Verdict::Other,
        })
    }

    fn parse_cert_kind(token: Option<&str>) -> Option<CertKind> {
        Some(match token? {
            "lower" => CertKind::Lower,
            "upper" => CertKind::Upper,
            "none" => CertKind::None,
            _ => CertKind::None,
        })
    }

    fn stage_sweep(line: &str) -> Option<(usize, usize)> {
        let rest = line.strip_prefix("fit stage ")?;
        let mut parts = rest.split_whitespace();
        let stage = parts.next()?.parse().ok()?;
        if parts.next()? != "sweep" {
            return None;
        }
        Some((stage, parts.next()?.parse().ok()?))
    }

    fn failure_label(line: &str) -> Option<&str> {
        if let Some((_, rest)) = line.split_once("analytic solve failed ") {
            return rest.split_whitespace().next();
        }
        const PREFIX: &str = "joint solve failed (";
        let at = line.find(PREFIX)? + PREFIX.len();
        let rest = &line[at..];
        let end = rest
            .find(") residual ")
            .or_else(|| rest.find(") damping "))?;
        Some(&rest[..end])
    }

    fn inexact_label(line: &str) -> Option<&str> {
        let at = line.find("inexact step (")? + "inexact step (".len();
        let rest = &line[at..];
        let end = rest.find(") residual ")?;
        Some(&rest[..end])
    }

    fn parse_trial(line: &str) -> Trial {
        let solve_status = match (bool_after(line, "stalled "), token_after(line, "failure ")) {
            (Some(false), Some("none")) => LinearSolveStatus::Success,
            (Some(true), _) => LinearSolveStatus::Failed,
            (_, Some(label)) if label != "none" => LinearSolveStatus::Failed,
            _ => LinearSolveStatus::Unreported,
        };
        Trial {
            trial_cost: number_after(line, "trial "),
            current: number_after(line, "current "),
            ratio: number_after(line, "ratio "),
            prediction: number_after_first(line, "prediction "),
            residual: number_after(line, "residual "),
            tolerance: number_after(line, "tolerance "),
            iterations: number_after(line, "iterations ").map(|value| value as usize),
            dim: number_after(line, "dim ").map(|value| value as usize),
            residual_coords: number_after(line, "residual_coords ").map(|value| value as usize),
            damping: number_after(line, "damping "),
            step: number_after(line, "step "),
            solve_status,
            trust: token_after(line, "solver ").map(|solver| TrustTrial {
                termination: match (solver, token_after(line, "termination ")) {
                    ("analytic-tr", Some("interior")) => TrustTermination::Interior,
                    ("analytic-tr", Some("boundary")) => TrustTermination::Boundary,
                    ("analytic-tr", Some("negative")) => TrustTermination::NegativeCurvature,
                    _ => TrustTermination::Invalid,
                },
                selected: match token_after(line, "selected ") {
                    Some("tr") => StepSelection::TrustRegion,
                    Some("cauchy") => StepSelection::Cauchy,
                    Some("box") => StepSelection::Box,
                    _ => StepSelection::Invalid,
                },
                proposal_residual: number_after(line, "proposal_residual "),
                radius: number_after(line, "radius "),
                scale: number_after(line, "scale "),
            }),
        }
    }

    fn returned_certificate(line: &str, sweep: usize) -> Certificate {
        let verdict = match token_after(line, "kind ") {
            Some("stationary") => Verdict::Stationary,
            Some("nonstationary") => Verdict::Nonstationary,
            _ => Verdict::Unresolved,
        };
        let kind = match verdict {
            Verdict::Stationary => CertKind::Upper,
            Verdict::Nonstationary => CertKind::Lower,
            _ => CertKind::None,
        };
        let lower = number_or_none_after(line, "lower ");
        let upper = number_or_none_after(line, "upper ");
        let value = match kind {
            CertKind::Upper => upper,
            CertKind::Lower => lower,
            CertKind::None => None,
        }
        .unwrap_or(f64::NAN);
        let complete = Some(
            token_after(line, "scope ") == Some("fullnative")
                && bool_after(line, "complete ") == Some(true),
        );
        let residual_coords = number_after(line, "residual_coords ").map(|value| value as usize);
        let iterations = number_after(line, "iterations ").map(|value| value as usize);
        let relative = number_or_none_after(line, "relative ");
        Certificate {
            kind,
            value,
            lower,
            complete,
            residual_coords,
            quadratic: None,
            first: None,
            residual_correction: None,
            iterations,
            relative,
            stalled: None,
            joint: number_after(line, "dim ").map(|value| value as usize),
            outer: None,
            border: None,
            verdict: Some(verdict),
            verdict_kind: Some(kind),
            verdict_termination: token_after(line, "termination ").map(str::to_owned),
            verdict_lower: lower,
            verdict_upper: upper,
            verdict_residual: relative,
            verdict_iterations: iterations,
            verdict_dim: number_after(line, "dim ").map(|value| value as usize),
            verdict_residual_coords: residual_coords,
            verdict_complete: complete,
            returned: true,
            sweep,
            raw: line.to_owned(),
        }
    }

    fn parse_diagnostics(lines: &[String]) -> Diag {
        let mut depth = 0usize;
        let mut scoped = false;
        let mut current = Vec::new();
        let mut frames = Vec::new();
        for line in lines {
            if line.starts_with("fit start ") {
                if depth == 0 {
                    current.clear();
                }
                depth += 1;
            }
            if depth == 1 {
                current.push(line);
            }
            if depth > 0 && line.starts_with("fit returned certificate ") {
                scoped = true;
                depth -= 1;
                if depth == 0 {
                    frames.push(parse_flat_diagnostics(current.iter().copied()));
                }
            }
        }
        if !scoped {
            return parse_flat_diagnostics(lines);
        }
        if depth > 0 {
            let mut unfinished = parse_flat_diagnostics(current);
            unfinished.certificate = None;
            unfinished.certificate_failure =
                Some("fit has no returned-state certificate".to_owned());
            frames.push(unfinished);
        }
        let mut chosen = frames
            .into_iter()
            .max_by_key(|diag| diag.samples.unwrap_or(0))
            .unwrap_or_default();
        chosen.fits = lines
            .iter()
            .filter(|line| line.starts_with("fit start "))
            .count();
        chosen.solve_failures.clear();
        for line in lines {
            if let Some(label) = failure_label(line) {
                *chosen.solve_failures.entry(label.to_owned()).or_default() += 1;
            }
            if let Some(label) = inexact_label(line) {
                *chosen
                    .solve_failures
                    .entry(format!("inexact:{label}"))
                    .or_default() += 1;
            }
        }
        chosen
    }

    fn parse_flat_diagnostics<'a>(lines: impl IntoIterator<Item = &'a String>) -> Diag {
        let mut all = Vec::new();
        let mut diag = Diag::default();
        let mut started = false;
        let mut current_sweep = 0usize;
        for line in lines {
            if line.starts_with("fit start ") {
                if started {
                    all.push(diag);
                    diag = Diag::default();
                }
                started = true;
                diag.fits = 1;
                diag.samples = number_after(line, "samples ").map(|value| value as usize);
                diag.max_stage = 0;
                diag.stage_start_cost = None;
                diag.last_cost = None;
                diag.last_cost_sweep = None;
                diag.accepted.clear();
                diag.last_accepted = None;
                diag.last_accepted_sweep = None;
                diag.last_rejected = None;
                diag.last_solve_residual = None;
                diag.last_solve_iterations = None;
                diag.last_solve_tolerance = None;
                diag.last_solve_stalled = None;
                diag.last_solve_failure = None;
                diag.solve_failures.clear();
                diag.last_joint_residual = None;
                diag.last_coupled = None;
                diag.certificate = None;
                diag.certificate_failure = None;
                diag.plateau = None;
                diag.termination = None;
                diag.gn_fallback = 0;
                diag.gn_failed = 0;
                current_sweep = 0;
                continue;
            }
            if let Some((stage, sweep)) = stage_sweep(line) {
                current_sweep = sweep;
                if let Some(cost) = number_after(line, "cost ") {
                    diag.last_cost = Some(cost);
                    diag.last_cost_sweep = Some(sweep);
                    if stage > diag.max_stage {
                        diag.max_stage = stage;
                        diag.stage_start_cost = Some(cost);
                    } else if stage == diag.max_stage && diag.stage_start_cost.is_none() {
                        diag.stage_start_cost = Some(cost);
                    }
                }
                if line.contains(" accepted trial ") {
                    let trial = parse_trial(line);
                    diag.last_solve_residual = trial.residual;
                    diag.last_solve_iterations = trial.iterations;
                    diag.last_solve_tolerance = trial.tolerance;
                    diag.last_solve_stalled = bool_after(line, "stalled ");
                    diag.last_solve_failure = token_after(line, "failure ")
                        .filter(|label| *label != "none")
                        .map(str::to_owned);
                    diag.certificate = None;
                    diag.certificate_failure = None;
                    diag.last_coupled = Some(trial.coupled());
                    diag.last_accepted_sweep = Some(current_sweep);
                    diag.accepted.push(trial.clone());
                    diag.last_accepted = Some(trial);
                } else if line.contains(" rejected trial ") {
                    diag.last_rejected = Some(parse_trial(line));
                }
                if line.contains(" termination ") {
                    diag.termination = token_after(line, "termination ").map(str::to_string);
                }
            }
            if line.starts_with("fit coupled residual ") {
                let relative = number_after(line, "relative ");
                let tolerance = number_after(line, "tolerance ");
                let iterations = number_after(line, "iterations ").map(|value| value as usize);
                let stalled = bool_after(line, "stalled ");
                let dim = number_after(line, "dim ").map(|value| value as usize);
                let residual_coords =
                    number_after(line, "residual_coords ").map(|value| value as usize);
                diag.last_solve_residual = relative;
                diag.last_solve_iterations = iterations;
                diag.last_solve_tolerance = tolerance;
                diag.last_solve_stalled = stalled;
                diag.last_coupled = Some(CoupledResidual {
                    dim,
                    residual_coords,
                });
            }
            if line.starts_with("fit returned certificate ") {
                diag.certificate = Some(returned_certificate(line, current_sweep));
                diag.certificate_failure = None;
            } else if line.starts_with("fit certificate verdict ") {
                if let Some(certificate) = diag.certificate.as_mut() {
                    certificate.verdict = parse_verdict(token_after(line, "verdict "));
                    certificate.verdict_kind = parse_cert_kind(token_after(line, "kind "));
                    certificate.verdict_termination =
                        token_after(line, "termination ").map(str::to_string);
                    certificate.verdict_lower = number_or_none_after(line, "lower ");
                    certificate.verdict_upper = number_or_none_after(line, "upper ");
                    certificate.verdict_residual = number_or_none_after(line, "residual ");
                    certificate.verdict_iterations =
                        number_after(line, "iterations ").map(|value| value as usize);
                    certificate.verdict_dim =
                        number_after(line, "dim ").map(|value| value as usize);
                    certificate.verdict_residual_coords =
                        number_after(line, "residual_coords ").map(|value| value as usize);
                    certificate.verdict_complete = bool_after(line, "complete ");
                    diag.certificate_failure = None;
                } else {
                    diag.certificate_failure = Some(line.clone());
                }
            } else if line.starts_with("fit certificate lower bound ") {
                let value = number_after(line, "lower bound ").unwrap_or(f64::NAN);
                diag.certificate = Some(Certificate {
                    kind: CertKind::Lower,
                    value,
                    lower: None,
                    complete: bool_after(line, "complete "),
                    residual_coords: number_after(line, "residual_coords ")
                        .map(|value| value as usize),
                    quadratic: None,
                    first: None,
                    residual_correction: None,
                    iterations: None,
                    relative: None,
                    stalled: None,
                    joint: number_after(line, "joint ").map(|value| value as usize),
                    outer: None,
                    border: None,
                    verdict: None,
                    verdict_kind: None,
                    verdict_termination: None,
                    verdict_lower: None,
                    verdict_upper: None,
                    verdict_residual: None,
                    verdict_iterations: None,
                    verdict_dim: None,
                    verdict_residual_coords: None,
                    verdict_complete: None,
                    returned: false,
                    sweep: current_sweep,
                    raw: line.clone(),
                });
                diag.certificate_failure = None;
            } else if line.starts_with("fit certificate bound ") {
                let value = number_after(line, "bound ").unwrap_or(f64::NAN);
                diag.certificate = Some(Certificate {
                    kind: CertKind::Upper,
                    value,
                    lower: number_after(line, "lower "),
                    complete: bool_after(line, "complete "),
                    residual_coords: number_after(line, "residual_coords ")
                        .map(|value| value as usize),
                    quadratic: number_after(line, "quadratic "),
                    first: number_after(line, "first "),
                    residual_correction: number_after(line, "residual "),
                    iterations: number_after(line, "iterations ").map(|value| value as usize),
                    relative: number_after(line, "relative "),
                    stalled: bool_after(line, "stalled "),
                    joint: number_after(line, "joint ").map(|value| value as usize),
                    outer: number_after(line, "outer ").map(|value| value as usize),
                    border: number_after(line, "border ").map(|value| value as usize),
                    verdict: None,
                    verdict_kind: None,
                    verdict_termination: None,
                    verdict_lower: None,
                    verdict_upper: None,
                    verdict_residual: None,
                    verdict_iterations: None,
                    verdict_dim: None,
                    verdict_residual_coords: None,
                    verdict_complete: None,
                    returned: false,
                    sweep: current_sweep,
                    raw: line.clone(),
                });
                diag.certificate_failure = None;
            } else if line.starts_with("fit certificate ") {
                diag.certificate = None;
                diag.certificate_failure = Some(line.clone());
            }
            if line.contains(" plateau decrement ") {
                diag.plateau = number_after(line, "plateau decrement ");
            }
            if let Some(label) = failure_label(line) {
                *diag.solve_failures.entry(label.to_string()).or_default() += 1;
                diag.last_solve_failure = Some(label.to_string());
                if let Some(value) = number_after(line, "residual ") {
                    diag.last_solve_residual = Some(value);
                }
                if let Some(value) = number_after(line, "tolerance ") {
                    diag.last_solve_tolerance = Some(value);
                }
            }
            if let Some(label) = inexact_label(line) {
                let key = format!("inexact:{label}");
                *diag.solve_failures.entry(key).or_default() += 1;
                diag.last_solve_failure = Some(format!("inexact:{label}"));
                if let Some(value) = number_after(line, "residual ") {
                    diag.last_solve_residual = Some(value);
                }
                if let Some(value) = number_after(line, "tolerance ") {
                    diag.last_solve_tolerance = Some(value);
                }
            }
            if line.starts_with("fit joint residual ") {
                diag.last_joint_residual = number_after(line, "joint residual ");
                if let Some(value) = number_after(line, "iterations ") {
                    diag.last_solve_iterations = Some(value as usize);
                }
                if let Some(value) = number_after(line, "tolerance ") {
                    diag.last_solve_tolerance = Some(value);
                }
                if bool_after(line, "stalled ").unwrap_or(false) {
                    diag.last_solve_failure = Some("work limit".to_string());
                }
            }
            if line.starts_with("fit checkpoint ") {
                if line.contains(" reason budget ") {
                    diag.termination = Some("budget".to_string());
                } else if line.contains(" reason joint ") {
                    diag.termination = Some("stalled".to_string());
                }
            }
            if line.contains(" not converged after ") {
                diag.termination = Some("max-sweeps".to_string());
            }
            if line.contains(" falling back to the GN curvature model") {
                diag.gn_fallback += 1;
            }
            if line.contains(" GN model failed: ") {
                diag.gn_failed += 1;
            }
        }
        if started {
            all.push(diag);
        }
        let fits = all.len();
        let mut chosen = all
            .into_iter()
            .max_by_key(|diag| diag.samples.unwrap_or(0))
            .unwrap_or_default();
        chosen.fits = fits;
        chosen
    }

    fn certificate_stale(diag: &Diag) -> bool {
        match (&diag.certificate, diag.last_accepted_sweep) {
            (Some(certificate), _) if certificate.returned => false,
            (Some(certificate), _) if certificate_complete(certificate) => true,
            (Some(certificate), Some(sweep)) => certificate.sweep <= sweep,
            _ => false,
        }
    }

    fn certificate_complete(certificate: &Certificate) -> bool {
        let complete = certificate
            .verdict_complete
            .or(certificate.complete)
            .unwrap_or(false);
        let residual_coords = certificate
            .verdict_residual_coords
            .or(certificate.residual_coords);
        complete && residual_coords.is_some()
    }

    fn solve_complete(diag: &Diag) -> bool {
        diag.last_accepted
            .as_ref()
            .is_some_and(|trial| trial.dim.is_some() && trial.residual_coords.is_some())
    }

    fn solve_ranked(diag: &Diag) -> bool {
        let Some(trial) = &diag.last_accepted else {
            return false;
        };
        if trial.dim.is_none()
            || trial.residual_coords.is_none()
            || trial.solve_status != LinearSolveStatus::Success
        {
            return false;
        }
        if let Some(trust) = &trial.trust {
            let valid_residual =
                |value: Option<f64>| value.is_some_and(|value| value.is_finite() && value >= 0.0);
            if !valid_residual(trial.residual)
                || !valid_residual(trust.proposal_residual)
                || !trust
                    .radius
                    .is_some_and(|value| value.is_finite() && value > 0.0)
                || !trust
                    .scale
                    .is_some_and(|value| value.is_finite() && value > 0.0 && value <= 1.0)
                || !trial
                    .prediction
                    .is_some_and(|value| value.is_finite() && value > 0.0)
            {
                return false;
            }
            return match (trust.selected, trust.termination) {
                (StepSelection::TrustRegion, TrustTermination::Interior) => {
                    match (trust.proposal_residual, trial.tolerance) {
                        (Some(residual), Some(tolerance)) => {
                            tolerance.is_finite() && tolerance > 0.0 && residual <= tolerance
                        }
                        _ => false,
                    }
                }
                (StepSelection::Cauchy | StepSelection::Box, TrustTermination::Interior)
                | (
                    StepSelection::Cauchy | StepSelection::Box | StepSelection::TrustRegion,
                    TrustTermination::Boundary | TrustTermination::NegativeCurvature,
                ) => true,
                _ => false,
            };
        }
        match (trial.residual, trial.tolerance) {
            (Some(residual), Some(tolerance)) => {
                residual.is_finite()
                    && residual >= 0.0
                    && tolerance.is_finite()
                    && tolerance > 0.0
                    && residual <= tolerance
            }
            _ => false,
        }
    }

    fn stationarity(diag: &Diag) -> Stationarity {
        let Some(certificate) = &diag.certificate else {
            return Stationarity::Unresolved;
        };
        let stale = certificate_stale(diag);
        let complete = certificate_complete(certificate);
        if let Some(verdict) = certificate.verdict {
            let upper_ok = certificate.verdict_upper.is_some_and(|value| {
                value.is_finite() && (0.0..=STATIONARY_DECREMENT_NATS).contains(&value)
            });
            let lower_ok = certificate
                .verdict_lower
                .is_some_and(|value| value.is_finite() && value > STATIONARY_DECREMENT_NATS);
            return match verdict {
                Verdict::Stationary
                    if complete
                        && !stale
                        && certificate.verdict_kind == Some(CertKind::Upper)
                        && upper_ok =>
                {
                    Stationarity::CertifiedStationary
                }
                Verdict::Nonstationary
                    if complete
                        && !stale
                        && certificate.verdict_kind == Some(CertKind::Lower)
                        && lower_ok =>
                {
                    Stationarity::CertifiedNonstationary
                }
                Verdict::Unresolved => Stationarity::Unresolved,
                _ => Stationarity::Uncertified,
            };
        }
        if !complete {
            return Stationarity::UncertifiedIncomplete;
        }
        if stale {
            return Stationarity::Uncertified;
        }
        match certificate.kind {
            CertKind::Upper
                if certificate.value.is_finite()
                    && (0.0..=STATIONARY_DECREMENT_NATS).contains(&certificate.value) =>
            {
                Stationarity::CertifiedStationary
            }
            CertKind::Lower
                if certificate.value.is_finite()
                    && certificate.value > STATIONARY_DECREMENT_NATS =>
            {
                Stationarity::CertifiedNonstationary
            }
            _ => Stationarity::Uncertified,
        }
    }

    fn failure_summary(failures: &BTreeMap<String, usize>) -> String {
        if failures.is_empty() {
            return "-".to_string();
        }
        failures
            .iter()
            .map(|(label, count)| format!("{}={count}", label.replace(' ', "-")))
            .collect::<Vec<_>>()
            .join(" ")
    }

    // ---- child execution ----

    struct ChildRun {
        fields: BTreeMap<String, String>,
        raw_result: Option<String>,
        lines: Vec<String>,
        wall: Duration,
        timed_out: bool,
        status: String,
    }

    fn parse_result_line(stdout: &str) -> Option<(BTreeMap<String, String>, String)> {
        let line = stdout
            .lines()
            .find(|line| line.starts_with("bounded-fit-result "))?;
        let rest = &line["bounded-fit-result ".len()..];
        let mut fields = BTreeMap::new();
        for field in rest.split(' ') {
            if let Some((key, value)) = field.split_once('=') {
                fields.insert(key.to_string(), value.to_string());
            }
        }
        if let Some(at) = rest.find(" note=") {
            fields.insert("note".to_string(), rest[at + " note=".len()..].to_string());
        }
        Some((fields, line.to_string()))
    }

    fn run_case(case: &Case, max_seconds: Option<f64>) -> ChildRun {
        let exe = std::env::current_exe().expect("current executable");
        let mut child = Command::new(exe)
            .arg("--child")
            .arg(case.name)
            .env("LOOKLAB_FIT_DIAG", "1")
            .env_remove("LOOKLAB_SYNTH_NO_NEUTRAL")
            .env_remove("LOOKLAB_MODEL_ERROR_DIAG")
            .env_remove("LOOKLAB_SIGMA_TRACE")
            .env_remove("LOOKLAB_TIMING")
            .env_remove("FIT_FD_STEP")
            .env_remove("LOOKLAB_FIT_TRIAL_SPLIT")
            .env_remove("LOOKLAB_FIT_CURVATURE")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn bounded-fit child");
        let stderr = child.stderr.take().expect("stderr pipe");
        let reader = std::thread::spawn(move || {
            let mut lines = Vec::new();
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                lines.push(line);
            }
            lines
        });
        let started = Instant::now();
        let mut timed_out = false;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(error) => {
                    eprintln!("bounded-fit-eval: wait on {} failed: {error}", case.name);
                    break;
                }
            }
            if max_seconds.is_some_and(|limit| started.elapsed() >= Duration::from_secs_f64(limit))
            {
                let _ = child.kill();
                let _ = child.wait();
                timed_out = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let wall = started.elapsed();
        let mut stdout = String::new();
        if let Some(mut pipe) = child.stdout.take() {
            let _ = pipe.read_to_string(&mut stdout);
        }
        let lines = reader.join().unwrap_or_default();
        let status = child
            .try_wait()
            .ok()
            .flatten()
            .and_then(|status| status.code())
            .map_or_else(|| "signal".to_string(), |code| format!("exit {code}"));
        let parsed = parse_result_line(&stdout);
        let (fields, raw_result) = match parsed {
            Some((fields, raw)) => (fields, Some(raw)),
            None => (BTreeMap::new(), None),
        };
        ChildRun {
            fields,
            raw_result,
            lines,
            wall,
            timed_out,
            status,
        }
    }

    // ---- cases ----

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum CaseKind {
        Fit,
        Call,
    }

    struct Case {
        name: &'static str,
        fixture: &'static str,
        kind: CaseKind,
        limit_seconds: f64,
        run: fn() -> FixtureResult,
    }

    const CASES: &[Case] = &[
        Case {
            name: "model-error-fold-0",
            fixture: "model_error::review_tests::single_body_pools_use_image_folds, kept images 2,3",
            kind: CaseKind::Fit,
            limit_seconds: 300.0,
            run: model_error_fold_0,
        },
        Case {
            name: "model-error-fold-1",
            fixture: "model_error::review_tests::single_body_pools_use_image_folds, kept images 0,1",
            kind: CaseKind::Fit,
            limit_seconds: 300.0,
            run: model_error_fold_1,
        },
        Case {
            name: "model-error-call",
            fixture: "model_error::review_tests::single_body_pools_use_image_folds, whole call",
            kind: CaseKind::Call,
            limit_seconds: 600.0,
            run: model_error_call,
        },
        Case {
            name: "controlled-recovery",
            fixture: "controlled::predicts_held_out_rerenders_at_unseen_settings, 22 library images",
            kind: CaseKind::Fit,
            limit_seconds: 600.0,
            run: controlled_recovery,
        },
    ];

    // ---- parent ----

    struct Options {
        diag: bool,
        list: bool,
        max_seconds: Option<f64>,
        names: Vec<String>,
    }

    fn usage() {
        println!(
            "\
bounded_fit_eval [--diag] [--max-seconds N] [--list] [case...]
bounded_fit_eval --replay-frozen STATE
bounded_fit_eval --diagnose-fixed-linearization STATE [--gradient-reference N]

Runs the bounded coupled-GN comparison cases, one child process per case,
with LOOKLAB_FIT_DIAG=1. Prints public-API quality/latent recovery and the parsed
objective, gain, solve residual, certificate kind/value and termination.

  --diag                    echo the child's LOOKLAB_FIT_DIAG lines
  --max-seconds N           per-case wall cap; 0 disables (default 300-600 s)
  --list                    list cases and exit
  --replay-frozen STATE     resume one frozen native state
  --diagnose-fixed-linearization STATE
                            one fixed-linearization TR/Cauchy/box comparison; omitting
                            --gradient-reference resets the reference and labels a
                            first-resume attempt
  --diagnose-fallback-ab STATE
                            frozen baseline/candidate diagonal-fallback comparison with one
                            shared linearization and a first-resume reference

Default with no case names: all cases. Native only; requires --features synth."
        );
    }

    fn parse_args(args: &[String]) -> Result<Options, String> {
        let mut options = Options {
            diag: false,
            list: false,
            max_seconds: None,
            names: Vec::new(),
        };
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--diag" => options.diag = true,
                "--list" => options.list = true,
                "--max-seconds" => {
                    let value = iter.next().ok_or("--max-seconds needs a value")?;
                    let seconds: f64 = value
                        .parse()
                        .map_err(|_| format!("--max-seconds {value} is not a number"))?;
                    options.max_seconds = Some(seconds);
                }
                other if other.starts_with("--") => {
                    return Err(format!("unknown option {other}"));
                }
                other => options.names.push(other.to_string()),
            }
        }
        Ok(options)
    }

    fn select(options: &Options) -> Result<Vec<&'static Case>, String> {
        if options.names.is_empty() {
            return Ok(CASES.iter().collect());
        }
        let mut selected = Vec::new();
        for name in &options.names {
            let case = CASES
                .iter()
                .find(|case| case.name == name)
                .ok_or_else(|| format!("unknown case {name}"))?;
            selected.push(case);
        }
        Ok(selected)
    }

    fn field<'a>(fields: &'a BTreeMap<String, String>, key: &str) -> &'a str {
        fields.get(key).map_or("-", String::as_str)
    }

    fn field_bool(fields: &BTreeMap<String, String>, key: &str) -> bool {
        fields.get(key).is_some_and(|value| value == "1")
    }

    struct GainSummary {
        accepted_decrease: f64,
        last: Option<f64>,
        min: Option<f64>,
        max: Option<f64>,
    }

    struct SolveSummary {
        complete: bool,
        ranked: bool,
    }

    fn print_report(
        case: &Case,
        run: &ChildRun,
        diag: &Diag,
        stationarity: Stationarity,
        pass: bool,
        gains: GainSummary,
        solve: SolveSummary,
    ) {
        println!("=== {} ===", case.name);
        println!("fixture: {}", case.fixture);
        if let Some(raw) = &run.raw_result {
            println!("raw result: {raw}");
        }
        println!(
            "status: {} (child {}{})",
            child_status(&run.fields).as_str(),
            run.status,
            if run.timed_out { ", timeout" } else { "" }
        );
        println!(
            "converged: {}  sweeps: {}  evaluations: {}  samples: {}",
            field(&run.fields, "converged"),
            field(&run.fields, "sweeps"),
            field(&run.fields, "evaluations"),
            field(&run.fields, "samples")
        );
        println!(
            "objective: final {}  stage-start {}  last-printed {} at sweep {}  accepted-decrease {:.9e}  accepted {}",
            field(&run.fields, "final_cost"),
            fmt_float(diag.stage_start_cost),
            fmt_float(diag.last_cost),
            fmt_count(diag.last_cost_sweep),
            gains.accepted_decrease,
            diag.accepted.len()
        );
        if let Some(trial) = &diag.last_accepted {
            println!(
                "last-accepted: decrease {}  ratio {}  prediction {}  residual {}  tolerance {}  iterations {}  dim {}  residual-coords {}  damping {}  step {}",
                fmt_float(trial.decrease()),
                fmt_float(trial.ratio),
                fmt_float(trial.prediction),
                fmt_float(trial.residual),
                fmt_float(trial.tolerance),
                fmt_count(trial.iterations),
                fmt_count(trial.dim),
                fmt_count(trial.residual_coords),
                fmt_float(trial.damping),
                fmt_float(trial.step)
            );
        } else if let Some(trial) = &diag.last_rejected {
            println!(
                "last-rejected: cost {}  current {}  ratio {}  prediction {}  damping {}  step {}",
                fmt_float(trial.trial_cost),
                fmt_float(trial.current),
                fmt_float(trial.ratio),
                fmt_float(trial.prediction),
                fmt_float(trial.damping),
                fmt_float(trial.step)
            );
        } else {
            println!("last-accepted: none");
        }
        println!(
            "solve: complete {}  dim {}  residual-coords {}  residual {}  iterations {}  tolerance {}  stalled {}  failure {}  failures {}  joint-residual {}  ranked {}",
            u8::from(solve.complete),
            diag.last_coupled
                .as_ref()
                .and_then(|coupled| coupled.dim)
                .map_or_else(|| "-".to_string(), |value| value.to_string()),
            diag.last_coupled
                .as_ref()
                .and_then(|coupled| coupled.residual_coords)
                .map_or_else(|| "-".to_string(), |value| value.to_string()),
            fmt_float(diag.last_solve_residual),
            fmt_count(diag.last_solve_iterations),
            fmt_float(diag.last_solve_tolerance),
            diag.last_solve_stalled
                .map_or_else(|| "-".to_string(), |value| u8::from(value).to_string()),
            diag.last_solve_failure.as_deref().unwrap_or("-"),
            failure_summary(&diag.solve_failures),
            fmt_float(diag.last_joint_residual),
            u8::from(solve.ranked)
        );
        match &diag.certificate {
            Some(certificate) => {
                println!(
                    "certificate: kind {}  value {}  lower {}  complete {}  residual-coords {}  iterations {}  sweep {}  stale {}  relative {}  joint {}  outer {}  border {}  residual-correction {}  quadratic {}  first {}  stalled {}  verdict {}  verdict-kind {}  verdict-termination {}  verdict-lower {}  verdict-upper {}  verdict-iterations {}",
                    cert_kind_str(certificate.kind),
                    fmt_plain(certificate.value),
                    fmt_float(certificate.lower),
                    certificate
                        .complete
                        .or(certificate.verdict_complete)
                        .map_or_else(|| "-".to_string(), |value| u8::from(value).to_string()),
                    fmt_count(
                        certificate
                            .residual_coords
                            .or(certificate.verdict_residual_coords),
                    ),
                    fmt_count(certificate.iterations),
                    certificate.sweep,
                    u8::from(certificate_stale(diag)),
                    fmt_float(certificate.verdict_residual.or(certificate.relative)),
                    fmt_count(certificate.verdict_dim.or(certificate.joint)),
                    fmt_count(certificate.outer),
                    fmt_count(certificate.border),
                    fmt_float(certificate.residual_correction),
                    fmt_float(certificate.quadratic),
                    fmt_float(certificate.first),
                    certificate
                        .stalled
                        .map_or_else(|| "-".to_string(), |value| u8::from(value).to_string()),
                    verdict_str(certificate.verdict),
                    cert_kind_opt_str(certificate.verdict_kind),
                    certificate.verdict_termination.as_deref().unwrap_or("-"),
                    fmt_float(certificate.verdict_lower),
                    fmt_float(certificate.verdict_upper),
                    fmt_count(certificate.verdict_iterations)
                );
                println!("raw certificate: {}", certificate.raw);
            }
            None => {
                println!(
                    "certificate: none  failure {}",
                    diag.certificate_failure.as_deref().unwrap_or("-")
                );
            }
        }
        println!(
            "plateau: decrement {}  threshold {:.3e}",
            fmt_float(diag.plateau),
            STATIONARY_DECREMENT_NATS
        );
        println!(
            "termination: {}  stationarity: {}  pass: {}",
            diag.termination.as_deref().unwrap_or("-"),
            stationarity.as_str(),
            u8::from(pass)
        );
        println!(
            "latent: median {}  p95 {}  gauge {}",
            field(&run.fields, "latent_median"),
            field(&run.fields, "latent_p95"),
            field(&run.fields, "gauge")
        );
        println!(
            "quality: {}  median {}  threshold {}  p95 {}  p95-threshold {}  pass {}",
            field(&run.fields, "quality"),
            field(&run.fields, "quality_median"),
            field(&run.fields, "quality_threshold"),
            field(&run.fields, "quality_p95"),
            field(&run.fields, "quality_p95_threshold"),
            field(&run.fields, "quality_pass")
        );
        println!(
            "diagnostics: hessian {}  operator-rows {}  objective-samples {}  gn-fallback {}  gn-failed {}",
            field(&run.fields, "hessian"),
            field(&run.fields, "operator_rows"),
            field(&run.fields, "objective_samples"),
            diag.gn_fallback,
            diag.gn_failed
        );
        println!(
            "runtime: child {} s  parent {} s  fits {}",
            field(&run.fields, "wall_s"),
            run.wall.as_secs_f64(),
            diag.fits
        );
        println!(
            "bounded-fit-eval fixture={} status={} converged={} termination={} stationarity={} pass={} accepted_decrease={:.9e} accepted={} gain_last={} gain_min={} gain_max={} solve_residual={} solve_iterations={} solve_tolerance={} solve_failure={} solve_failures={} joint_residual={} certificate_kind={} certificate_value={} certificate_lower={} certificate_complete={} certificate_residual_coords={} certificate_verdict={} certificate_verdict_kind={} certificate_verdict_termination={} solve_complete={} solve_dim={} solve_residual_coords={} solve_stalled=- solve_ranked={} certificate_sweep={} certificate_stale={} certificate_relative={} certificate_joint={} certificate_outer={} certificate_border={} certificate_residual_correction={} plateau={} latent_median={} latent_p95={} gauge={} quality={} quality_median={} quality_threshold={} quality_p95={} quality_p95_threshold={} quality_pass={} evidence={} wall_s={:.3} note={}",
            case.name,
            child_status(&run.fields).as_str(),
            field(&run.fields, "converged"),
            diag.termination.as_deref().unwrap_or("-"),
            stationarity.as_str(),
            u8::from(pass),
            gains.accepted_decrease,
            diag.accepted.len(),
            fmt_float(gains.last),
            fmt_float(gains.min),
            fmt_float(gains.max),
            fmt_float(diag.last_solve_residual),
            fmt_count(diag.last_solve_iterations),
            fmt_float(diag.last_solve_tolerance),
            diag.last_solve_failure.as_deref().unwrap_or("-"),
            failure_summary(&diag.solve_failures),
            fmt_float(diag.last_joint_residual),
            diag.certificate
                .as_ref()
                .map_or("-", |certificate| cert_kind_str(certificate.kind)),
            diag.certificate.as_ref().map_or_else(
                || "-".to_string(),
                |certificate| fmt_plain(certificate.value)
            ),
            diag.certificate.as_ref().map_or_else(
                || "-".to_string(),
                |certificate| fmt_float(certificate.lower)
            ),
            diag.certificate.as_ref().map_or_else(
                || "-".to_string(),
                |certificate| {
                    certificate
                        .complete
                        .or(certificate.verdict_complete)
                        .map_or_else(|| "-".to_string(), |value| u8::from(value).to_string())
                },
            ),
            diag.certificate
                .as_ref()
                .and_then(|certificate| {
                    certificate
                        .residual_coords
                        .or(certificate.verdict_residual_coords)
                })
                .map_or_else(|| "-".to_string(), |value| value.to_string()),
            diag.certificate
                .as_ref()
                .map_or("-", |certificate| verdict_str(certificate.verdict)),
            diag.certificate
                .as_ref()
                .map_or("-", |certificate| cert_kind_opt_str(
                    certificate.verdict_kind
                )),
            diag.certificate
                .as_ref()
                .and_then(|certificate| certificate.verdict_termination.as_deref())
                .unwrap_or("-"),
            u8::from(solve.complete),
            diag.last_accepted
                .as_ref()
                .and_then(|trial| trial.dim)
                .map_or_else(|| "-".to_string(), |value| value.to_string()),
            diag.last_accepted
                .as_ref()
                .and_then(|trial| trial.residual_coords)
                .map_or_else(|| "-".to_string(), |value| value.to_string()),
            u8::from(solve.ranked),
            diag.certificate.as_ref().map_or_else(
                || "-".to_string(),
                |certificate| certificate.sweep.to_string()
            ),
            u8::from(certificate_stale(diag)),
            diag.certificate.as_ref().map_or_else(
                || "-".to_string(),
                |certificate| { fmt_float(certificate.verdict_residual.or(certificate.relative)) }
            ),
            diag.certificate
                .as_ref()
                .and_then(|certificate| certificate.verdict_dim.or(certificate.joint))
                .map_or_else(|| "-".to_string(), |value| value.to_string()),
            diag.certificate
                .as_ref()
                .and_then(|certificate| certificate.outer)
                .map_or_else(|| "-".to_string(), |value| value.to_string()),
            diag.certificate
                .as_ref()
                .and_then(|certificate| certificate.border)
                .map_or_else(|| "-".to_string(), |value| value.to_string()),
            diag.certificate.as_ref().map_or_else(
                || "-".to_string(),
                |certificate| fmt_float(certificate.residual_correction)
            ),
            fmt_float(diag.plateau),
            field(&run.fields, "latent_median"),
            field(&run.fields, "latent_p95"),
            field(&run.fields, "gauge"),
            field(&run.fields, "quality"),
            field(&run.fields, "quality_median"),
            field(&run.fields, "quality_threshold"),
            field(&run.fields, "quality_p95"),
            field(&run.fields, "quality_p95_threshold"),
            field(&run.fields, "quality_pass"),
            field(&run.fields, "evidence"),
            run.wall.as_secs_f64(),
            field(&run.fields, "note"),
        );
        println!();
    }

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            write!(&mut out, "{:02x}", *byte).expect("write to string");
        }
        out
    }

    fn fmt_optional(value: Option<f64>) -> String {
        value.map_or_else(|| "-".to_string(), fmt_plain)
    }

    fn fmt_sigma(values: &[f64]) -> String {
        values
            .iter()
            .map(|value| format!("{:.17e}", *value))
            .collect::<Vec<_>>()
            .join(",")
    }

    fn fmt_lambdas(lambdas: &looklab_fit::StageLambdas) -> String {
        format!(
            "tone:{:.17e},tint:{:.17e},grl:{:.17e},residual:{:.17e},response_tone:{:.17e},response_tint:{:.17e},response_chroma:{:.17e},response_hue:{:.17e},response_lightness:{:.17e},response_wb:{:.17e}",
            lambdas.tone,
            lambdas.tint,
            lambdas.grl,
            lambdas.residual,
            lambdas.response_tone,
            lambdas.response_tint,
            lambdas.response_chroma,
            lambdas.response_hue,
            lambdas.response_lightness,
            lambdas.response_wb,
        )
    }

    fn fixed_linearization_input(
        path: &str,
        gradient_reference: Option<f64>,
    ) -> Result<
        (
            String,
            usize,
            FitState,
            looklab_fit::FixedLinearizationInput,
        ),
        String,
    > {
        let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
        let input_sha = hex(Sha256::digest(&bytes).as_slice());
        let state: FitState = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        if state.stage != 4 {
            return Err(format!(
                "fixed-linearization input stage {} is not the final stage 4",
                state.stage
            ));
        }
        let options = controlled_options();
        let sigma_log = state
            .plateau
            .objective
            .as_ref()
            .map(|objective| objective.sigma.clone());
        if let Some(objective) = state.plateau.objective.as_ref() {
            if objective.stage != state.stage {
                return Err(format!(
                    "plateau stage {} differs from checkpoint stage {}",
                    objective.stage, state.stage
                ));
            }
            if objective.lambdas != state.lambdas {
                return Err("plateau lambdas differ from checkpoint lambdas".to_string());
            }
            if objective.residual != options.fit_residual {
                return Err("plateau residual flag differs from the controlled options".to_string());
            }
        }
        let input = looklab_fit::FixedLinearizationInput {
            theta: state.theta.clone(),
            lambdas: state.lambdas,
            sigma_log,
            trust_radius: state.trust_radius,
            gradient_reference,
            checkpoint_generation: state.generation.0,
            checkpoint_stage: state.stage,
            checkpoint_sweep: state.sweep,
        };
        Ok((input_sha, bytes.len(), state, input))
    }

    fn run_fixed_linearization_diagnostic(
        path: &str,
        gradient_reference: Option<f64>,
    ) -> Result<(), String> {
        let (input_sha, input_bytes, state, input) =
            fixed_linearization_input(path, gradient_reference)?;
        let source_generation =
            std::env::var("LOOKLAB_SOURCE_GENERATION").unwrap_or_else(|_| "unset".to_string());
        let config = controlled_config();
        let options = controlled_options();
        let library = library_settings();
        let dataset = controlled_dataset(&config);
        let train = &dataset.images[..library.len()];
        println!(
            "bounded-fixed-linearization-input path={path} sha256={input_sha} bytes={input_bytes} source_sha256={source_generation} gradient_reference={:?} checkpoint_generation={} checkpoint_stage={} checkpoint_sweep={} converged={} damping={:.17e} theta={}",
            gradient_reference,
            state.generation.0,
            state.stage,
            state.sweep,
            u8::from(state.converged),
            state.damping,
            state.theta.len(),
        );
        let report = looklab_fit::diagnose_fixed_linearization(
            train,
            &dataset.schema,
            1,
            &sim(),
            &options,
            &input,
        )
        .map_err(|error| error.to_string())?;
        let note = if report.note.is_empty() {
            "-".to_string()
        } else {
            report.note.replace('\n', " ")
        };
        let trace_json = serde_json::to_string(&report.trace).map_err(|error| error.to_string())?;
        let ordinary_cost = report
            .ordinary_trial
            .as_ref()
            .map_or_else(|| "-".to_string(), |trial| fmt_plain(trial.cost));
        let ordinary_decrease = report
            .ordinary_trial
            .as_ref()
            .map_or_else(|| "-".to_string(), |trial| fmt_plain(trial.decrease));
        let ordinary_ratio = report
            .ordinary_trial
            .as_ref()
            .map_or_else(|| "-".to_string(), |trial| fmt_plain(trial.ratio));
        let ordinary_infeasible = report.ordinary_trial.as_ref().map_or_else(
            || "-".to_string(),
            |trial| u8::from(trial.trial_infeasible).to_string(),
        );
        let ordinary_accepted = report.ordinary_trial.as_ref().map_or_else(
            || "-".to_string(),
            |trial| u8::from(trial.accepted).to_string(),
        );
        let box_cost = report
            .box_trial
            .as_ref()
            .map_or_else(|| "-".to_string(), |trial| fmt_plain(trial.cost));
        let box_decrease = report
            .box_trial
            .as_ref()
            .map_or_else(|| "-".to_string(), |trial| fmt_plain(trial.decrease));
        let box_ratio = report
            .box_trial
            .as_ref()
            .map_or_else(|| "-".to_string(), |trial| fmt_plain(trial.ratio));
        let box_infeasible = report.box_trial.as_ref().map_or_else(
            || "-".to_string(),
            |trial| u8::from(trial.trial_infeasible).to_string(),
        );
        let box_accepted = report.box_trial.as_ref().map_or_else(
            || "-".to_string(),
            |trial| u8::from(trial.accepted).to_string(),
        );
        let winner_trial = match report.winner {
            looklab_fit::FixedLinearizationWinner::TrustRegion
            | looklab_fit::FixedLinearizationWinner::Cauchy => report.ordinary_trial.as_ref(),
            looklab_fit::FixedLinearizationWinner::Box => report.box_trial.as_ref(),
            looklab_fit::FixedLinearizationWinner::None => None,
        };
        let winner_accepted = winner_trial.map_or_else(
            || "-".to_string(),
            |trial| u8::from(trial.accepted).to_string(),
        );
        println!(
            "bounded-fixed-linearization-result status={} disposition={} note={} checkpoint_generation={} checkpoint_stage={} checkpoint_sweep={} images={} samples={} excluded={} excluded_samples={} coordinates={} joint_dim={} residual_coordinates={} sigma_log={} lambdas={} cost={} infeasible={} hessian_rows={} active_samples={} radius={} gradient_norm={} gradient_reference={} reference_source={} tolerance={} tr_termination={} tr_iterations={} tr_relative_residual={} tr_scale={} tr_limiter={} tr_prediction={} tr_metric_norm={} tr_largest={} tr_available={} tr_coordinate_feasible={} tr_radius_feasible={} partial_recursive_prediction={} partial_metric_norm={} partial_largest={} partial_coordinate_feasible={} partial_radius_feasible={} cauchy_alpha={} cauchy_limiter={} cauchy_prediction={} cauchy_metric_norm={} cauchy_largest={} cauchy_available={} cauchy_coordinate_feasible={} cauchy_radius_feasible={} ordinary={} ordinary_prediction={} ordinary_cost={} ordinary_decrease={} ordinary_ratio={} ordinary_trial_infeasible={} ordinary_accepted={} box_source={} box_clipped={} box_prediction={} box_metric_norm={} box_largest={} box_available={} box_coordinate_feasible={} box_radius_feasible={} box_descent={} box_cost={} box_decrease={} box_ratio={} box_trial_infeasible={} box_accepted={} box_actual_improvement={} winner={} winner_prediction={} winner_accepted={} linearization_passes={} objective_passes={} sample_evaluations={} inner_products={} post_products={} total_products={} trace_json={}",
            report.status.label(),
            report.disposition.label(),
            note,
            report.checkpoint_generation,
            report.checkpoint_stage,
            report.checkpoint_sweep,
            report.images,
            report.samples,
            report.excluded,
            report.excluded_samples,
            report.coordinates,
            report.joint_dim,
            report.residual_coordinates,
            fmt_sigma(&report.sigma_log),
            fmt_lambdas(&report.lambdas),
            fmt_plain(report.cost),
            u8::from(report.infeasible),
            report.hessian_rows,
            report.active_samples,
            fmt_plain(report.radius),
            fmt_plain(report.gradient_norm),
            fmt_plain(report.gradient_reference),
            report.reference_source.label(),
            fmt_plain(report.tolerance),
            report.tr_termination.unwrap_or("-"),
            report.tr_iterations,
            fmt_plain(report.tr_relative_residual),
            fmt_plain(report.tr_scale),
            report.tr_limiter.unwrap_or("-"),
            fmt_optional(report.tr.prediction),
            fmt_optional(report.tr.metric_norm),
            fmt_optional(report.tr.largest),
            u8::from(report.tr.available),
            u8::from(report.tr.coordinate_feasible),
            u8::from(report.tr.radius_feasible),
            report.partial.as_ref().map_or_else(
                || "-".to_string(),
                |partial| fmt_optional(partial.recursive_prediction)
            ),
            report
                .partial
                .as_ref()
                .map_or_else(|| "-".to_string(), |partial| fmt_plain(partial.metric_norm)),
            report
                .partial
                .as_ref()
                .map_or_else(|| "-".to_string(), |partial| fmt_plain(partial.largest)),
            report.partial.as_ref().map_or_else(
                || "-".to_string(),
                |partial| u8::from(partial.coordinate_feasible).to_string(),
            ),
            report.partial.as_ref().map_or_else(
                || "-".to_string(),
                |partial| u8::from(partial.radius_feasible).to_string(),
            ),
            fmt_plain(report.cauchy_alpha),
            report.cauchy_limiter.unwrap_or("-"),
            fmt_optional(report.cauchy.prediction),
            fmt_optional(report.cauchy.metric_norm),
            fmt_optional(report.cauchy.largest),
            u8::from(report.cauchy.available),
            u8::from(report.cauchy.coordinate_feasible),
            u8::from(report.cauchy.radius_feasible),
            report.ordinary.label(),
            fmt_optional(report.ordinary_candidate.prediction),
            ordinary_cost,
            ordinary_decrease,
            ordinary_ratio,
            ordinary_infeasible,
            ordinary_accepted,
            report.box_source.label(),
            report.box_clipped,
            fmt_optional(report.box_candidate.prediction),
            fmt_optional(report.box_candidate.metric_norm),
            fmt_optional(report.box_candidate.largest),
            u8::from(report.box_candidate.available),
            u8::from(report.box_candidate.coordinate_feasible),
            u8::from(report.box_candidate.radius_feasible),
            u8::from(report.box_candidate.descent()),
            box_cost,
            box_decrease,
            box_ratio,
            box_infeasible,
            box_accepted,
            fmt_plain(report.box_actual_improvement),
            report.winner.label(),
            fmt_optional(report.winner_candidate.prediction),
            winner_accepted,
            report.operations.linearization_passes,
            report.operations.objective_passes,
            report.operations.sample_evaluations,
            report.operations.inner_products,
            report.operations.post_products,
            report.operations.total_products,
            trace_json,
        );
        if report.status.completed() {
            Ok(())
        } else {
            Err(format!(
                "fixed-linearization status {}",
                report.status.label()
            ))
        }
    }

    fn fmt_full(value: f64) -> String {
        if value.is_finite() {
            format!("{value:.17e}")
        } else if value.is_nan() {
            "nan".to_string()
        } else {
            "inf".to_string()
        }
    }

    fn fmt_full_optional(value: Option<f64>) -> String {
        value.map_or_else(|| "-".to_string(), fmt_full)
    }

    fn fmt_group_choices(groups: &[looklab_fit::PreconditionerGroupChoice]) -> String {
        if groups.is_empty() {
            return "-".to_string();
        }
        groups
            .iter()
            .map(|group| format!("{}+{}:{}", group.start, group.width, group.choice.label()))
            .collect::<Vec<_>>()
            .join(",")
    }

    fn print_fallback_arm(report: &looklab_fit::FallbackArmReport) {
        let note = if report.note.is_empty() {
            "-".to_string()
        } else {
            report.note.replace('\n', " ")
        };
        let trace_json = serde_json::to_string(&report.trace).unwrap_or_else(|_| "-".to_string());
        let trial = report.trial;
        println!(
            "bounded-fallback-ab-arm arm={} status={} note={} work_limit={} groups={} tr_termination={} tr_iterations={} tr_relative_residual={} tr_scale={} tr_limiter={} tr_prediction={} tr_metric_norm={} tr_largest={} tr_available={} tr_coordinate_feasible={} tr_radius_feasible={} cauchy_alpha={} cauchy_limiter={} cauchy_prediction={} cauchy_metric_norm={} cauchy_largest={} cauchy_available={} cauchy_coordinate_feasible={} cauchy_radius_feasible={} box_source={} box_clipped={} box_prediction={} box_metric_norm={} box_largest={} box_available={} box_coordinate_feasible={} box_radius_feasible={} ordinary={} winner={} winner_prediction={} winner_metric_norm={} winner_largest={} winner_available={} winner_coordinate_feasible={} winner_radius_feasible={} trial_cost={} trial_decrease={} trial_prediction={} trial_gain={} trial_infeasible={} trial_accepted={} inner_products={} post_products={} total_products={} sample_evaluations={} trace={}",
            report.arm.label(),
            report.status.label(),
            note,
            report.work_limit,
            fmt_group_choices(&report.groups),
            report.tr_termination.unwrap_or("-"),
            report.tr_iterations,
            fmt_full(report.tr_relative_residual),
            fmt_full(report.tr_scale),
            report.tr_limiter.unwrap_or("-"),
            fmt_full_optional(report.tr.prediction),
            fmt_full_optional(report.tr.metric_norm),
            fmt_full_optional(report.tr.largest),
            u8::from(report.tr.available),
            u8::from(report.tr.coordinate_feasible),
            u8::from(report.tr.radius_feasible),
            fmt_full(report.cauchy_alpha),
            report.cauchy_limiter.unwrap_or("-"),
            fmt_full_optional(report.cauchy.prediction),
            fmt_full_optional(report.cauchy.metric_norm),
            fmt_full_optional(report.cauchy.largest),
            u8::from(report.cauchy.available),
            u8::from(report.cauchy.coordinate_feasible),
            u8::from(report.cauchy.radius_feasible),
            report.box_source.label(),
            report.box_clipped,
            fmt_full_optional(report.box_candidate.prediction),
            fmt_full_optional(report.box_candidate.metric_norm),
            fmt_full_optional(report.box_candidate.largest),
            u8::from(report.box_candidate.available),
            u8::from(report.box_candidate.coordinate_feasible),
            u8::from(report.box_candidate.radius_feasible),
            report.ordinary.label(),
            report.winner.label(),
            fmt_full_optional(report.winner_candidate.prediction),
            fmt_full_optional(report.winner_candidate.metric_norm),
            fmt_full_optional(report.winner_candidate.largest),
            u8::from(report.winner_candidate.available),
            u8::from(report.winner_candidate.coordinate_feasible),
            u8::from(report.winner_candidate.radius_feasible),
            trial.map_or_else(|| "-".to_string(), |trial| fmt_full(trial.cost)),
            trial.map_or_else(|| "-".to_string(), |trial| fmt_full(trial.decrease)),
            trial.map_or_else(|| "-".to_string(), |trial| fmt_full(trial.prediction)),
            trial.map_or_else(|| "-".to_string(), |trial| fmt_full(trial.ratio)),
            trial.map_or_else(
                || "-".to_string(),
                |trial| u8::from(trial.trial_infeasible).to_string(),
            ),
            trial.map_or_else(
                || "-".to_string(),
                |trial| u8::from(trial.accepted).to_string(),
            ),
            report.inner_products,
            report.post_products,
            report.total_products,
            report.sample_evaluations,
            trace_json,
        );
    }

    fn run_fallback_ab(path: &str) -> Result<(), String> {
        const EXPECTED_INPUT_SHA: &str =
            "4e5e48a2b636eb127c3d192fe66df6c3a162a847afdd6f74d18ce101f89289ab";
        const EXPECTED_RADIUS: f64 = 5.930281951623323;

        let (input_sha, input_bytes, state, input) = fixed_linearization_input(path, None)?;
        if input_sha != EXPECTED_INPUT_SHA {
            return Err(format!("fallback A/B input SHA256 differs: {input_sha}"));
        }
        if std::env::var_os("LOOKLAB_SYNTH_NO_NEUTRAL").is_some() {
            return Err(
                "fallback A/B refuses LOOKLAB_SYNTH_NO_NEUTRAL: it changes the fixture".to_string(),
            );
        }
        let source_generation =
            std::env::var("LOOKLAB_SOURCE_GENERATION").unwrap_or_else(|_| "unset".to_string());
        if source_generation == "unset" || source_generation.is_empty() {
            return Err("fallback A/B requires LOOKLAB_SOURCE_GENERATION".to_string());
        }
        let Some(saved_radius) = state.trust_radius else {
            return Err("fallback A/B checkpoint has no saved trust radius".to_string());
        };
        if saved_radius.get().to_bits() != EXPECTED_RADIUS.to_bits() {
            return Err(format!(
                "fallback A/B saved trust radius is {:.17e}, expected {EXPECTED_RADIUS:.17e}",
                saved_radius.get()
            ));
        }
        let config = controlled_config();
        let options = controlled_options();
        let library = library_settings();
        let dataset = controlled_dataset(&config);
        let train = &dataset.images[..library.len()];
        println!(
            "bounded-fallback-ab-input path={path} sha256={input_sha} bytes={input_bytes} source_sha256={source_generation} checkpoint_generation={} checkpoint_stage={} checkpoint_sweep={} converged={} damping={:.17e} theta={} radius={}",
            state.generation.0,
            state.stage,
            state.sweep,
            u8::from(state.converged),
            state.damping,
            state.theta.len(),
            fmt_full(saved_radius.get()),
        );
        let report =
            looklab_fit::diagnose_fallback_ab(train, &dataset.schema, 1, &sim(), &options, &input)
                .map_err(|error| error.to_string())?;
        let note = if report.note.is_empty() {
            "-".to_string()
        } else {
            report.note.replace('\n', " ")
        };
        println!(
            "bounded-fallback-ab-shared comparison={} note={} checkpoint_generation={} checkpoint_stage={} checkpoint_sweep={} images={} samples={} excluded={} excluded_samples={} coordinates={} joint_dim={} residual_coordinates={} sigma_log={} lambdas={} cost={} infeasible={} hessian_rows={} active_samples={} radius={} gradient_norm={} gradient_reference={} reference_source={} tolerance={}",
            report.comparison.label(),
            note,
            report.checkpoint_generation,
            report.checkpoint_stage,
            report.checkpoint_sweep,
            report.images,
            report.samples,
            report.excluded,
            report.excluded_samples,
            report.coordinates,
            report.joint_dim,
            report.residual_coordinates,
            fmt_sigma(&report.sigma_log),
            fmt_lambdas(&report.lambdas),
            fmt_full(report.cost),
            u8::from(report.infeasible),
            report.hessian_rows,
            report.active_samples,
            fmt_full(report.radius),
            fmt_full(report.gradient_norm),
            fmt_full(report.gradient_reference),
            report.reference_source.label(),
            fmt_full(report.tolerance),
        );
        print_fallback_arm(&report.baseline);
        print_fallback_arm(&report.candidate);
        let baseline_mismatches = if report.baseline_mismatches.is_empty() {
            "-".to_string()
        } else {
            report.baseline_mismatches.join(",")
        };
        println!(
            "bounded-fallback-ab-result comparison={} baseline_decrease={} candidate_decrease={} tau={} improvement={} baseline_recorded_decrease={} baseline_recorded_tolerance={} baseline_recorded_cost={} baseline_recorded_prediction={} baseline_recorded_gain={} baseline_recorded_termination={} baseline_recorded_inner_products={} baseline_recorded_post_products={} baseline_recorded_radius={} baseline_recorded_largest={} baseline_recorded_selection={} baseline_recorded_clipping={} baseline_mismatches={} baseline_reproduced={} candidate_finite={} candidate_feasible={} candidate_accepted={} candidate_exceeds_tau={} candidate_improves={} accounting_complete={} shared_allowance_binding={} sample_ceiling_respected={} product_ceiling_respected={} operations_linearization_passes={} operations_objective_passes={} operations_sample_evaluations={} operations_inner_products={} operations_post_products={} operations_total_products={} shared_inner_allowance={} reserved_post_products={} total_product_ceiling={} sample_ceiling={}",
            report.comparison.label(),
            fmt_full(report.baseline_decrease),
            fmt_full(report.candidate_decrease),
            fmt_full(report.tau),
            fmt_full(report.improvement),
            fmt_full(report.baseline_recorded_decrease),
            fmt_full(report.baseline_recorded_tolerance),
            fmt_full(report.baseline_recorded_cost),
            fmt_full(report.baseline_recorded_prediction),
            fmt_full(report.baseline_recorded_gain),
            report.baseline_recorded_termination,
            report.baseline_recorded_inner_products,
            report.baseline_recorded_post_products,
            fmt_full(report.baseline_recorded_radius),
            fmt_full(report.baseline_recorded_largest),
            report.baseline_recorded_selection,
            report.baseline_recorded_clipping,
            baseline_mismatches,
            u8::from(report.baseline_reproduced),
            u8::from(report.candidate_finite),
            u8::from(report.candidate_feasible),
            u8::from(report.candidate_accepted),
            u8::from(report.candidate_exceeds_tau),
            u8::from(report.candidate_improves),
            u8::from(report.accounting_complete),
            u8::from(report.shared_allowance_binding),
            u8::from(report.sample_ceiling_respected),
            u8::from(report.product_ceiling_respected),
            report.operations.linearization_passes,
            report.operations.objective_passes,
            report.operations.sample_evaluations,
            report.operations.inner_products,
            report.operations.post_products,
            report.operations.total_products,
            4088,
            8,
            4096,
            10080,
        );
        if report.completed() {
            Ok(())
        } else {
            Err(format!(
                "fallback A/B comparison {}",
                report.comparison.label()
            ))
        }
    }

    pub fn main() -> ExitCode {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.iter().any(|arg| arg == "--help" || arg == "-h") {
            usage();
            return ExitCode::SUCCESS;
        }
        if args
            .first()
            .is_some_and(|arg| arg == "--diagnose-fixed-linearization")
        {
            let Some(path) = args.get(1) else {
                eprintln!("bounded-fit-eval: --diagnose-fixed-linearization needs a state path");
                return ExitCode::from(2);
            };
            let mut gradient_reference = None;
            let mut index = 2;
            while let Some(argument) = args.get(index) {
                match argument.as_str() {
                    "--gradient-reference" => {
                        let Some(value) = args.get(index + 1) else {
                            eprintln!("bounded-fit-eval: --gradient-reference needs a value");
                            return ExitCode::from(2);
                        };
                        let value: f64 = match value.parse() {
                            Ok(value) => value,
                            Err(_) => {
                                eprintln!(
                                    "bounded-fit-eval: --gradient-reference {value} is not a number"
                                );
                                return ExitCode::from(2);
                            }
                        };
                        gradient_reference = Some(value);
                        index += 2;
                    }
                    other => {
                        eprintln!(
                            "bounded-fit-eval: unknown --diagnose-fixed-linearization argument {other}"
                        );
                        return ExitCode::from(2);
                    }
                }
            }
            return match run_fixed_linearization_diagnostic(path, gradient_reference) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("bounded fixed-linearization diagnostic failed: {error}");
                    ExitCode::from(1)
                }
            };
        }
        if args
            .first()
            .is_some_and(|arg| arg == "--diagnose-fallback-ab")
        {
            let Some(path) = args.get(1) else {
                eprintln!("bounded-fit-eval: --diagnose-fallback-ab needs a state path");
                return ExitCode::from(2);
            };
            if let Some(other) = args.get(2) {
                eprintln!("bounded-fit-eval: unknown --diagnose-fallback-ab argument {other}");
                return ExitCode::from(2);
            }
            return match run_fallback_ab(path) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("bounded fallback A/B failed: {error}");
                    ExitCode::from(1)
                }
            };
        }
        if args.first().is_some_and(|arg| arg == "--replay-frozen") {
            let Some(path) = args.get(1) else {
                eprintln!("bounded-fit-eval: --replay-frozen needs a state path");
                return ExitCode::from(2);
            };
            return match replay_frozen_native(path) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("bounded frozen replay failed: {error}");
                    ExitCode::from(1)
                }
            };
        }
        if args.first().is_some_and(|arg| arg == "--child") {
            let Some(name) = args.get(1) else {
                eprintln!("bounded-fit-eval: --child needs a name");
                return ExitCode::from(2);
            };
            let Some(case) = CASES.iter().find(|case| case.name == name) else {
                eprintln!("bounded-fit-eval: unknown case {name}");
                return ExitCode::from(2);
            };
            let result = (case.run)();
            print_child(&result);
            if CAPTURE_FAILED.load(Ordering::Relaxed) {
                eprintln!("bounded-fit-eval: checkpoint capture failed");
                return ExitCode::from(1);
            }
            return ExitCode::SUCCESS;
        }
        let options = match parse_args(&args) {
            Ok(options) => options,
            Err(message) => {
                eprintln!("bounded-fit-eval: {message}");
                usage();
                return ExitCode::from(2);
            }
        };
        if options.list {
            for case in CASES {
                println!("{:<22} {}", case.name, case.fixture);
            }
            return ExitCode::SUCCESS;
        }
        let cases = match select(&options) {
            Ok(cases) => cases,
            Err(message) => {
                eprintln!("bounded-fit-eval: {message}");
                return ExitCode::from(2);
            }
        };
        if cases.is_empty() {
            println!("bounded-fit-eval: no cases selected");
            return ExitCode::SUCCESS;
        }
        println!("bounded-fit-eval: {} case(s)", cases.len());
        let started = Instant::now();
        let mut passes = 0usize;
        for case in &cases {
            let limit = options
                .max_seconds
                .map_or(Some(case.limit_seconds), |seconds| {
                    (seconds > 0.0).then_some(seconds)
                });
            let run = run_case(case, limit);
            if options.diag {
                for line in &run.lines {
                    println!("[diag] {line}");
                }
            }
            let mut diag = parse_diagnostics(&run.lines);
            let accepted_decrease: f64 = diag.accepted.iter().filter_map(Trial::decrease).sum();
            let mut gains: Vec<f64> = diag
                .accepted
                .iter()
                .filter_map(|trial| trial.ratio)
                .filter(|ratio| ratio.is_finite())
                .collect();
            gains.sort_by(f64::total_cmp);
            let gain_last = diag.last_accepted.as_ref().and_then(|trial| trial.ratio);
            let gain_min = gains.first().copied();
            let gain_max = gains.last().copied();
            let converged = field_bool(&run.fields, "converged");
            let quality_pass = field_bool(&run.fields, "quality_pass");
            if case.kind == CaseKind::Call {
                diag.termination = Some(if converged {
                    "call-ok".to_string()
                } else {
                    "call-no-evidence".to_string()
                });
            } else if diag.termination.is_none() {
                diag.termination = Some(if converged {
                    "certified".to_string()
                } else {
                    "returned-unconverged".to_string()
                });
            }
            let solve_complete = solve_complete(&diag);
            let solve_ranked = solve_ranked(&diag);
            let stationarity = if case.kind == CaseKind::Call {
                Stationarity::NotApplicable
            } else if child_status(&run.fields) != ChildStatus::Ok {
                Stationarity::Unresolved
            } else {
                stationarity(&diag)
            };
            let status_ok = child_status(&run.fields) == ChildStatus::Ok;
            let child_ok = run.status == "exit 0" && !run.timed_out;
            let pass = child_ok
                && status_ok
                && converged
                && quality_pass
                && (case.kind == CaseKind::Call
                    || (stationarity == Stationarity::CertifiedStationary && solve_ranked));
            if pass {
                passes += 1;
            }
            print_report(
                case,
                &run,
                &diag,
                stationarity,
                pass,
                GainSummary {
                    accepted_decrease,
                    last: gain_last,
                    min: gain_min,
                    max: gain_max,
                },
                SolveSummary {
                    complete: solve_complete,
                    ranked: solve_ranked,
                },
            );
        }
        println!(
            "bounded-fit-eval: {passes}/{} passed in {:.1} s",
            cases.len(),
            started.elapsed().as_secs_f64()
        );
        if passes == cases.len() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn lines(values: &[&str]) -> Vec<String> {
            values.iter().map(|value| (*value).to_owned()).collect()
        }

        #[test]
        fn only_reported_successful_accurate_steps_are_ranked() {
            for (status, residual, expected) in [
                ("stalled 0 failure none", "0.00001", true),
                ("stalled 1 failure work-limit", "0.00001", false),
                ("stalled 0 failure none", "0.01", false),
                ("", "0.00001", false),
            ] {
                let trace = format!(
                    "fit stage 4 sweep 1 accepted trial 10 ratio 0.5 prediction 2 residual {residual} tolerance 0.001 {status} dim 7 residual_coords 3"
                );
                let diag = parse_diagnostics(&lines(&["fit start images 2 samples 400", &trace]));
                assert_eq!(solve_ranked(&diag), expected);
            }
        }

        #[test]
        fn trust_truncation_is_distinct_from_an_inaccurate_interior_solve() {
            for (selected, termination, proposal, expected) in [
                ("tr", "boundary", "0.8", true),
                ("tr", "negative", "0.8", true),
                ("tr", "interior", "0.8", false),
                ("tr", "interior", "0.00001", true),
                ("tr", "unknown", "0.00001", false),
                ("cauchy", "interior", "0.8", true),
                ("box", "interior", "0.8", true),
                ("box", "boundary", "0.8", true),
                ("box", "negative", "0.8", true),
                ("box", "unknown", "0.00001", false),
                ("unknown", "boundary", "0.8", false),
            ] {
                let trace = format!(
                    "fit stage 4 sweep 1 accepted trial 10 ratio 0.5 prediction 2 residual 0.9 tolerance 0.001 stalled 0 failure none dim 7 residual_coords 3 solver analytic-tr termination {termination} proposal_residual {proposal} radius 1 scale 0.5 selected {selected}"
                );
                let diag = parse_diagnostics(&lines(&["fit start images 2 samples 400", &trace]));
                assert_eq!(solve_ranked(&diag), expected);
                assert!(stationarity(&diag) == Stationarity::Unresolved);
            }
        }

        #[test]
        fn accepted_motion_invalidates_an_earlier_complete_bound() {
            let diag = parse_diagnostics(&lines(&[
                "fit start images 2 samples 400",
                "fit certificate bound 0.0001 lower 0 complete 1 residual_coords 3",
                "fit stage 4 sweep 1 accepted trial 10 ratio 0.5 prediction 2 residual 0.00001 tolerance 0.001 stalled 0 failure none dim 7 residual_coords 3",
            ]));
            assert!(stationarity(&diag) == Stationarity::Unresolved);
        }

        #[test]
        fn unresolved_return_replaces_an_earlier_stationary_bound() {
            let diag = parse_diagnostics(&lines(&[
                "fit start images 2 samples 400",
                "fit certificate bound 0.0001 lower 0 complete 1 residual_coords 3",
                "fit returned certificate kind unresolved scope fullnative lower none upper none complete 0 dim none residual_coords none iterations none relative none termination infeasible",
            ]));
            assert!(stationarity(&diag) == Stationarity::Unresolved);
        }

        #[test]
        fn nested_fit_cannot_replace_the_parent_state_or_step() {
            let diag = parse_diagnostics(&lines(&[
                "fit start images 2 samples 400",
                "fit stage 4 sweep 1 accepted trial 10 ratio 0.5 prediction 2 residual 0.00001 tolerance 0.001 stalled 0 failure none dim 7 residual_coords 3",
                "fit start images 1 samples 200",
                "fit stage 4 sweep 2 joint solve failed (work limit) residual 0.2 tolerance 0.001",
                "fit stage 4 sweep 2 accepted trial 5 ratio 0.5 prediction 1 residual 0.000001 tolerance 0.001 stalled 0 failure none dim 12 residual_coords 8",
                "fit returned certificate kind stationary scope fullnative lower 0 upper 0.00001 complete 1 dim 12 residual_coords 8 iterations 4 relative 0.000001 termination none",
                "fit returned certificate kind nonstationary scope fullnative lower 0.2 upper 0.3 complete 1 dim 7 residual_coords 3 iterations 4 relative 0.000001 termination none",
            ]));
            assert!(stationarity(&diag) == Stationarity::CertifiedNonstationary);
            assert_eq!(
                diag.last_accepted.as_ref().and_then(|trial| trial.dim),
                Some(7)
            );
            assert_eq!(diag.solve_failures.get("work limit"), Some(&1));
            assert_eq!(diag.fits, 2);
        }

        #[test]
        fn returned_bound_requires_full_scope_and_nonnegative_energy() {
            for (scope, upper, expected) in [
                ("fullnative", "0.00001", Stationarity::CertifiedStationary),
                ("conditional", "0.00001", Stationarity::Uncertified),
                ("fullnative", "-1", Stationarity::Uncertified),
            ] {
                let trace = format!(
                    "fit returned certificate kind stationary scope {scope} lower 0 upper {upper} complete 1 dim 7 residual_coords 3 iterations 4 relative 0.000001 termination none"
                );
                let diag = parse_diagnostics(&lines(&["fit start images 2 samples 400", &trace]));
                assert!(stationarity(&diag) == expected);
            }
        }
    }
}

#[cfg(feature = "synth")]
fn main() -> std::process::ExitCode {
    bounded::main()
}

#[cfg(not(feature = "synth"))]
fn main() -> std::process::ExitCode {
    eprintln!("bounded_fit_eval requires --features synth");
    std::process::ExitCode::from(2)
}
