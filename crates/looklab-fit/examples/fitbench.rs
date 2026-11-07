// SPDX-License-Identifier: AGPL-3.0-or-later
//! Convergence bench for the fit cases that fail on the current tree.
//!
//! Each case runs in a child of this binary with `LOOKLAB_FIT_DIAG` set, because the
//! stationarity certificate, the damping, the gain ratio and the joint-solve failure kinds
//! are only reported on that stream. The parent parses them, prints one row per case and
//! exits nonzero when any case did not converge, so it can drive `git bisect run`.
//!
//! Fixtures are copied from the failing tests. The LUT-import case is absent:
//! `looklab-export` depends on `looklab-fit`, so reaching `import_lut_look` from here
//! would need a dependency cycle.
//!
//! Native only: it spawns processes, reads pipes and measures wall time.
#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

#[cfg(feature = "synth")]
mod bench {
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Read};
    use std::process::{Command, ExitCode, Stdio};
    use std::time::{Duration, Instant};

    use looklab_fit::synth::{
        self, ControlSynthConfig, LookSelector, SynthConfig, controlled_dataset,
    };
    use looklab_fit::{ControlValue, FitImage, FitOptions, Settings, fit, fit_controlled};
    use looklab_schema::{ControlKey, SimKey, validate_control_schema};

    const SCHEMA: &str = include_str!("../../../data/bodies/fujifilm-x-t5.yaml");
    const FOLD_CLIP_K: f64 = 0.03;

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
            ControlValue::Enum(looklab_schema::EnumValue::new(value).expect("enum value")),
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

    fn capped_options() -> FitOptions {
        FitOptions {
            hessian_samples: 997,
            max_sweeps: FitOptions::default().max_sweeps,
            ..controlled_options()
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

    /// Worker defaults from `looklab-worker`'s `refit`: its own seed, no sample cap, no budget.
    fn worker_options() -> FitOptions {
        FitOptions {
            seed: 0x5eed_1a5e,
            max_samples_per_image: 10_000,
            sweep_budget: f64::MAX,
            ..FitOptions::default()
        }
    }

    fn mislabelled_look(truth: &looklab_model::Look) -> looklab_model::Look {
        let mut look = synth::perturbed_look(truth);
        for value in &mut look.tint.a {
            *value += 6.0;
        }
        for value in &mut look.color.g.volume {
            *value += 0.35;
        }
        for value in &mut look.color.r.volume {
            *value += 20.0;
        }
        look
    }

    // ---- cases ----

    struct Outcome {
        converged: bool,
        sweeps: Option<usize>,
        evaluations: Option<usize>,
        note: String,
    }

    fn from_fit(result: looklab_fit::Result<looklab_fit::FitResult>) -> Outcome {
        match result {
            Ok(result) => Outcome {
                converged: result.diagnostics.converged,
                sweeps: Some(result.diagnostics.sweeps),
                evaluations: Some(result.diagnostics.evaluations),
                note: String::new(),
            },
            Err(error) => Outcome {
                converged: false,
                sweeps: None,
                evaluations: None,
                note: format!("error: {error}"),
            },
        }
    }

    /// Fold 0 fits images 2 and 3; fold 1 fits images 0 and 1.
    fn model_error_fold(fold: usize) -> Outcome {
        let data = model_error_dataset();
        let start = fold * 2;
        let kept: Vec<FitImage> = data
            .images
            .iter()
            .enumerate()
            .filter(|(index, _)| !(start..start + 2).contains(index))
            .map(|(_, image)| image.clone())
            .collect();
        from_fit(fit(&kept, &model_error_options()))
    }

    fn model_error_fold_0() -> Outcome {
        model_error_fold(0)
    }

    fn model_error_fold_1() -> Outcome {
        model_error_fold(1)
    }

    fn model_error_test() -> Outcome {
        let data = model_error_dataset();
        match looklab_fit::estimate_model_error(
            &data.images,
            &[],
            None,
            &model_error_options(),
            FOLD_CLIP_K,
        ) {
            Ok(field) => {
                let held_out = field.held_out_mean_rms.iter().any(|value| *value > 0.0);
                Outcome {
                    converged: held_out,
                    sweeps: None,
                    evaluations: None,
                    note: if held_out {
                        String::new()
                    } else {
                        "held-out mean rms is zero".to_string()
                    },
                }
            }
            Err(error) => Outcome {
                converged: false,
                sweeps: None,
                evaluations: None,
                note: format!("error: {error}"),
            },
        }
    }

    /// The inner reference fit of `capped_hessian_rows_converge_on_a_noisy_fit`: the six
    /// library images with empty settings. `score_controlled` skips the probe uncertainty
    /// that `fit` would run, and that path estimates model error on the same six images.
    fn reference_six() -> Outcome {
        let dataset = controlled_dataset(&controlled_config());
        let epoch = dataset.schema.epochs.first().expect("schema epoch");
        match looklab_fit::score_controlled(
            &dataset.images[..6],
            &dataset.schema,
            epoch,
            &sim(),
            &capped_options(),
        ) {
            Ok(scores) => Outcome {
                converged: scores.diagnostics.converged,
                sweeps: Some(scores.diagnostics.sweeps),
                evaluations: Some(scores.diagnostics.evaluations),
                note: String::new(),
            },
            Err(error) => Outcome {
                converged: false,
                sweeps: None,
                evaluations: None,
                note: format!("error: {error}"),
            },
        }
    }

    fn controlled_twenty_eight() -> Outcome {
        let dataset = controlled_dataset(&controlled_config());
        let library = library_settings();
        from_fit(fit_controlled(
            &dataset.images[..library.len()],
            &dataset.schema,
            1,
            &sim(),
            &capped_options(),
        ))
    }

    /// The pool of `a_firmware_split_opens_a_worker_proposal_with_evidence`: fifteen images
    /// of one Look and fifteen of a mislabelled Look, without the database around them.
    fn worker_pool_thirty() -> Outcome {
        let config = SynthConfig {
            seed: 21,
            images: 30,
            samples_per_image: 300,
            ..SynthConfig::default()
        };
        let truth = synth::truth_look(config.seed);
        let shifted = mislabelled_look(&truth);
        let base = synth::dataset(&config);
        let minority = synth::dataset_with(&config, &shifted, &LookSelector::Single(&shifted));
        let mut pool: Vec<FitImage> = base.images[..15].to_vec();
        pool.extend(minority.images[15..].iter().cloned());
        from_fit(fit_controlled(
            &pool,
            &schema(),
            1,
            &sim(),
            &worker_options(),
        ))
    }

    struct Case {
        name: &'static str,
        fixture: &'static str,
        slow: bool,
        /// Whether the row's certificate, damping, ratio and failure counts describe the fit.
        fit_metrics: bool,
        /// Whether the row reports the model-error phase timing split.
        phases: bool,
        /// Per-case wall cap, unless `--max-seconds` overrides it.
        limit_seconds: f64,
        run: fn() -> Outcome,
    }

    const CASES: &[Case] = &[
        Case {
            name: "model-error-fold-0",
            fixture: "model_error::review_tests::single_body_pools_use_image_folds, kept images 2,3",
            slow: false,
            fit_metrics: true,
            phases: true,
            limit_seconds: 300.0,
            run: model_error_fold_0,
        },
        Case {
            name: "model-error-fold-1",
            fixture: "model_error::review_tests::single_body_pools_use_image_folds, kept images 0,1",
            slow: false,
            fit_metrics: true,
            phases: true,
            limit_seconds: 300.0,
            run: model_error_fold_1,
        },
        Case {
            name: "model-error-test",
            fixture: "model_error::review_tests::single_body_pools_use_image_folds, whole call",
            slow: false,
            fit_metrics: false,
            phases: true,
            limit_seconds: 600.0,
            run: model_error_test,
        },
        Case {
            name: "reference-6",
            fixture: "controlled::capped_hessian_rows_converge_on_a_noisy_fit, inner reference fit",
            slow: true,
            fit_metrics: true,
            phases: false,
            limit_seconds: 1200.0,
            run: reference_six,
        },
        Case {
            name: "controlled-28",
            fixture: "controlled::capped_hessian_rows_converge_on_a_noisy_fit, main fit",
            slow: true,
            fit_metrics: true,
            phases: false,
            limit_seconds: 1800.0,
            run: controlled_twenty_eight,
        },
        Case {
            name: "worker-pool-30",
            fixture: "worker::a_firmware_split_opens_a_worker_proposal_with_evidence, pool",
            slow: false,
            fit_metrics: true,
            phases: false,
            limit_seconds: 600.0,
            run: worker_pool_thirty,
        },
    ];

    // ---- diagnostics parsing ----

    #[derive(Clone, Default)]
    struct Sweep {
        sweep: usize,
        cost: Option<f64>,
        certificate: Option<f64>,
        ratio: Option<f64>,
        damping: Option<f64>,
        iterations: Option<usize>,
        residual: Option<f64>,
        decrement: Option<f64>,
    }

    struct Nuisance {
        converged: bool,
        attempts: usize,
        damping: f64,
    }

    #[derive(Default)]
    struct Metrics {
        certificate: Option<f64>,
        damping: Option<f64>,
        ratio: Option<f64>,
        failures: BTreeMap<SolveKind, usize>,
        nuisance: Option<Nuisance>,
        sweeps: Vec<Sweep>,
        folds: Vec<String>,
        starts: Vec<String>,
        fits: usize,
        phases: Phases,
    }

    fn token_after<'a>(line: &'a str, key: &str) -> Option<&'a str> {
        let at = line.rfind(key)? + key.len();
        line[at..].split_whitespace().next()
    }

    fn number_after(line: &str, key: &str) -> Option<f64> {
        token_after(line, key).and_then(|token| token.parse().ok())
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
        const PREFIX: &str = "joint solve failed (";
        let at = line.find(PREFIX)? + PREFIX.len();
        let rest = &line[at..];
        let end = rest
            .find(") residual ")
            .or_else(|| rest.find(") damping "))?;
        Some(&rest[..end])
    }

    fn certificate_value(line: &str) -> Option<f64> {
        if line.starts_with("fit certificate lower bound ") {
            return number_after(line, "lower bound ");
        }
        if line.starts_with("fit certificate bound ") {
            return number_after(line, "bound ");
        }
        Some(f64::INFINITY)
    }

    fn sweep_entry(sweeps: &mut Vec<Sweep>, sweep: usize) -> &mut Sweep {
        if !sweeps.last().is_some_and(|last| last.sweep == sweep) {
            sweeps.push(Sweep {
                sweep,
                ..Sweep::default()
            });
        }
        sweeps.last_mut().expect("just pushed")
    }

    /// Wall time inside each model-error phase, from the arrival time of the diagnostic line
    /// that opens and closes it: `fit start` to the fold's `kept converged` line is the fold
    /// Look fit, and that line to the last `nuisance` line is the nuisance conditioning.
    #[derive(Default)]
    struct Phases {
        fits: Duration,
        nuisance: Duration,
        has_nuisance: bool,
    }

    fn phases(lines: &[(Duration, String)]) -> Phases {
        let mut out = Phases::default();
        let mut fit_start: Option<Duration> = None;
        let mut nuisance_start: Option<Duration> = None;
        let mut nuisance_end: Option<Duration> = None;
        for (at, line) in lines {
            if line.starts_with("fit start ") {
                if let Some(start) = fit_start.take() {
                    out.fits += at.saturating_sub(start);
                }
                fit_start = Some(*at);
            } else if line.starts_with("model_error fold ") && line.contains("kept converged=") {
                if let Some(start) = fit_start.take() {
                    out.fits += at.saturating_sub(start);
                }
                nuisance_start = Some(*at);
                nuisance_end = None;
            } else if line.starts_with("nuisance ") {
                out.has_nuisance = true;
                nuisance_start.get_or_insert(*at);
                nuisance_end = Some(*at);
            }
        }
        if let (Some(start), Some(end)) = (nuisance_start, nuisance_end) {
            out.nuisance += end.saturating_sub(start);
        }
        if let Some(start) = fit_start
            && let Some((last, _)) = lines.last()
        {
            out.fits += last.saturating_sub(start);
        }
        out
    }

    fn parse_diagnostics(lines: &[(Duration, String)]) -> Metrics {
        let mut metrics = Metrics {
            phases: phases(lines),
            ..Metrics::default()
        };
        let mut current: Option<usize> = None;
        for (_, line) in lines {
            if line.starts_with("fit start ") {
                // A case can run several fits (the inner reference fit, then the main fit);
                // the row describes the last one.
                metrics.fits += 1;
                metrics.starts.push(line.to_string());
                metrics.certificate = None;
                metrics.damping = None;
                metrics.ratio = None;
                metrics.failures.clear();
                metrics.sweeps.clear();
                current = None;
                continue;
            }
            if line.starts_with("fit certificate ") {
                let value = certificate_value(line);
                metrics.certificate = value;
                if let Some(sweep) = current {
                    sweep_entry(&mut metrics.sweeps, sweep).certificate = value;
                }
                continue;
            }
            if line.starts_with("nuisance done ") {
                metrics.nuisance = Some(Nuisance {
                    converged: token_after(line, "converged ")
                        .and_then(|token| token.parse().ok())
                        .unwrap_or(false),
                    attempts: token_after(line, "attempts ")
                        .and_then(|token| token.parse().ok())
                        .unwrap_or(0),
                    damping: number_after(line, "damping ").unwrap_or(f64::NAN),
                });
                continue;
            }
            if line.starts_with("model_error fold ") {
                metrics.folds.push(line.to_string());
                continue;
            }
            if let Some((_, sweep)) = stage_sweep(line) {
                current = Some(sweep);
                let entry = sweep_entry(&mut metrics.sweeps, sweep);
                if line.contains(" cost ") {
                    entry.cost = number_after(line, "cost ");
                }
                if line.contains("plateau decrement ") {
                    entry.decrement = number_after(line, "plateau decrement ");
                }
                if line.contains(" accepted trial ") || line.contains(" rejected trial ") {
                    entry.ratio = number_after(line, "ratio ");
                    entry.iterations =
                        token_after(line, "iterations ").and_then(|token| token.parse().ok());
                    entry.residual = number_after(line, "residual ");
                }
                if let Some(ratio) = entry.ratio {
                    metrics.ratio = Some(ratio);
                }
            }
            if let Some(label) = failure_label(line) {
                *metrics.failures.entry(SolveKind::parse(label)).or_default() += 1;
            }
            if let Some(damping) = number_after(line, "damping ") {
                metrics.damping = Some(damping);
                if let Some(sweep) = current {
                    sweep_entry(&mut metrics.sweeps, sweep).damping = Some(damping);
                }
            }
        }
        metrics
    }

    // ---- formatting ----

    fn sci(value: Option<f64>) -> String {
        match value {
            Some(value) if value.is_finite() => format!("{value:.3e}"),
            Some(_) => "inf".to_string(),
            None => "-".to_string(),
        }
    }

    fn fixed(value: Option<f64>) -> String {
        match value {
            Some(value) if value.is_finite() => format!("{value:.3}"),
            _ => "-".to_string(),
        }
    }

    fn count(value: Option<usize>) -> String {
        value.map_or_else(|| "-".to_string(), |value| value.to_string())
    }

    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum SolveKind {
        WorkLimit,
        NonpositiveCurvature,
        FactorizationImage,
        FactorizationBorder,
        Factorization,
        Nonfinite,
        Other(String),
    }

    impl SolveKind {
        fn parse(label: &str) -> Self {
            match label {
                "work limit" => Self::WorkLimit,
                "nonpositive curvature" => Self::NonpositiveCurvature,
                "factorization (image)" => Self::FactorizationImage,
                "factorization (border)" => Self::FactorizationBorder,
                "factorization" => Self::Factorization,
                "nonfinite arithmetic" => Self::Nonfinite,
                other => Self::Other(other.to_string()),
            }
        }

        fn code(&self) -> &str {
            match self {
                Self::WorkLimit => "wl",
                Self::NonpositiveCurvature => "npc",
                Self::FactorizationImage => "fi",
                Self::FactorizationBorder => "fb",
                Self::Factorization => "fac",
                Self::Nonfinite => "nf",
                Self::Other(label) => label,
            }
        }
    }

    fn failure_summary(failures: &BTreeMap<SolveKind, usize>) -> String {
        if failures.is_empty() {
            return "-".to_string();
        }
        failures
            .iter()
            .map(|(kind, count)| format!("{}={count}", kind.code()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn nuisance_summary(nuisance: &Option<Nuisance>) -> String {
        match nuisance {
            None => "-".to_string(),
            Some(nuisance) if nuisance.converged => {
                format!("converged(attempts={})", nuisance.attempts)
            }
            Some(nuisance) => format!(
                "not-converged(attempts={},damping={:.1e})",
                nuisance.attempts, nuisance.damping
            ),
        }
    }

    const HEADERS: [&str; 10] = [
        "case",
        "converged",
        "sweeps",
        "evals",
        "E(nats)",
        "damping",
        "gain",
        "joint-failures",
        "nuisance",
        "wall-s",
    ];

    struct Row {
        cells: [String; 10],
        converged: bool,
        sweeps: Vec<Sweep>,
        detail: Vec<String>,
    }

    fn print_table(rows: &[Row]) {
        let mut widths = HEADERS.map(str::len);
        for row in rows {
            for (index, cell) in row.cells.iter().enumerate() {
                widths[index] = widths[index].max(cell.len());
            }
        }
        let line = |cells: &[&str]| {
            cells
                .iter()
                .enumerate()
                .map(|(index, cell)| format!("{cell:<width$}", width = widths[index]))
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_string()
        };
        println!("{}", line(&HEADERS));
        for row in rows {
            let cells: Vec<&str> = row.cells.iter().map(String::as_str).collect();
            println!("{}", line(&cells));
            for detail in &row.detail {
                println!("  {detail}");
            }
        }
    }

    fn print_trace(rows: &[Row]) {
        for row in rows {
            if row.sweeps.is_empty() {
                continue;
            }
            println!();
            println!("trace {}", row.cells[0]);
            println!(
                "  {:>5}  {:<13}  {:<12}  {:<8}  {:<10}  {:>4}  {:<9}  {:<9}",
                "sweep", "cost", "E(nats)", "gain", "damping", "cg", "residual", "decrement"
            );
            for sweep in &row.sweeps {
                println!(
                    "  {:>5}  {:<13}  {:<12}  {:<8}  {:<10}  {:>4}  {:<9}  {:<9}",
                    sweep.sweep,
                    sci(sweep.cost),
                    sci(sweep.certificate),
                    fixed(sweep.ratio),
                    sci(sweep.damping),
                    count(sweep.iterations),
                    sci(sweep.residual),
                    sci(sweep.decrement),
                );
            }
        }
    }

    // ---- child execution ----

    struct ChildRun {
        result: Option<Outcome>,
        lines: Vec<(Duration, String)>,
        wall: Duration,
        timed_out: bool,
        status: String,
    }

    fn parse_result(stdout: &str) -> Option<Outcome> {
        let line = stdout
            .lines()
            .find(|line| line.starts_with("fitbench-result "))?;
        let mut converged = false;
        let mut sweeps = None;
        let mut evaluations = None;
        let mut note = String::new();
        for field in line["fitbench-result ".len()..].split(' ') {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            match key {
                "converged" => converged = value == "1",
                "sweeps" => sweeps = value.parse().ok(),
                "evaluations" => evaluations = value.parse().ok(),
                _ => {}
            }
        }
        if let Some(at) = line.find(" note=") {
            note = line[at + " note=".len()..].to_string();
        }
        Some(Outcome {
            converged,
            sweeps,
            evaluations,
            note,
        })
    }

    fn run_case(case: &Case, max_seconds: Option<f64>) -> ChildRun {
        let exe = std::env::current_exe().expect("current executable");
        let mut child = Command::new(exe)
            .arg("--case")
            .arg(case.name)
            .env("LOOKLAB_FIT_DIAG", "1")
            .env("LOOKLAB_MODEL_ERROR_DIAG", "1")
            .env_remove("LOOKLAB_SYNTH_NO_NEUTRAL")
            .env_remove("LOOKLAB_SIGMA_TRACE")
            .env_remove("LOOKLAB_TIMING")
            .env_remove("FIT_FD_STEP")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn fitbench case");
        let stderr = child.stderr.take().expect("stderr pipe");
        let started = Instant::now();
        let reader_started = started;
        let reader = std::thread::spawn(move || {
            let mut lines = Vec::new();
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                lines.push((reader_started.elapsed(), line));
            }
            lines
        });
        let mut timed_out = false;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(error) => {
                    eprintln!("fitbench: wait on {} failed: {error}", case.name);
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
        let stderr = reader.join().unwrap_or_default();
        let status = child
            .try_wait()
            .ok()
            .flatten()
            .and_then(|status| status.code())
            .map_or_else(|| "signal".to_string(), |code| format!("exit {code}"));
        ChildRun {
            result: parse_result(&stdout),
            lines: stderr,
            wall,
            timed_out,
            status,
        }
    }

    // ---- parent ----

    struct Options {
        trace: bool,
        slow: bool,
        all: bool,
        list: bool,
        max_seconds: Option<f64>,
        names: Vec<String>,
    }

    fn usage() {
        println!(
            "fitbench [--trace] [--slow | --all] [--max-seconds N] [--list] [case...]\n\
             \n\
             Runs each failing fit case in its own process and prints one row per case.\n\
             Exits nonzero when any case did not converge.\n\
             \n\
             --trace            print per-sweep cost, certificate, gain, damping, CG and residual\n\
             --slow             run only the slow group\n\
             --all              run every case\n\
             --max-seconds N    stop a case after N seconds and report it non-converged\n\
                                (0 disables; default is 300-1800 per case)\n\
             --list             list the cases and exit\n\
             \n\
             The LUT import case is not here: looklab-export depends on looklab-fit, so reaching\n\
             import_lut_look from this example would need a dependency cycle."
        );
    }

    fn parse_args(args: &[String]) -> Result<Options, String> {
        let mut options = Options {
            trace: false,
            slow: false,
            all: false,
            list: false,
            max_seconds: None,
            names: Vec::new(),
        };
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--trace" => options.trace = true,
                "--slow" => options.slow = true,
                "--all" => options.all = true,
                "--list" => options.list = true,
                "--help" | "-h" => return Err(String::new()),
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
        let mut selected = Vec::new();
        if options.names.is_empty() {
            for case in CASES {
                let wanted = if options.all {
                    true
                } else if options.slow {
                    case.slow
                } else {
                    !case.slow
                };
                if wanted {
                    selected.push(case);
                }
            }
            return Ok(selected);
        }
        for name in &options.names {
            let case = CASES
                .iter()
                .find(|case| case.name == name)
                .ok_or_else(|| format!("unknown case {name}"))?;
            selected.push(case);
        }
        Ok(selected)
    }

    fn build_row(case: &Case, run: ChildRun, trace: bool) -> Row {
        let metrics = parse_diagnostics(&run.lines);
        let outcome = run.result;
        let converged = !run.timed_out && outcome.as_ref().is_some_and(|o| o.converged);
        let converged_cell = if run.timed_out {
            format!("false (timeout {:.0}s)", run.wall.as_secs_f64())
        } else {
            match &outcome {
                Some(outcome) if outcome.converged => "true".to_string(),
                _ => "false".to_string(),
            }
        };
        let mut detail = Vec::new();
        if let Some(start) = metrics.starts.last() {
            detail.push(start.clone());
        }
        if metrics.fits > 1 {
            detail.push(format!("fits={} (row describes the last)", metrics.fits));
        }
        if let Some(outcome) = &outcome
            && !outcome.note.is_empty()
        {
            detail.push(outcome.note.clone());
        } else if outcome.is_none() && !run.timed_out {
            detail.push(format!("no result line ({})", run.status));
        }
        detail.extend(metrics.folds.iter().cloned());
        if case.phases {
            let rest = run
                .wall
                .saturating_sub(metrics.phases.fits)
                .saturating_sub(metrics.phases.nuisance);
            detail.push(format!(
                "timing: fits={:.1}s nuisance={} aggregation={:.1}s",
                metrics.phases.fits.as_secs_f64(),
                if metrics.phases.has_nuisance {
                    format!("{:.1}s", metrics.phases.nuisance.as_secs_f64())
                } else {
                    "-".to_string()
                },
                rest.as_secs_f64(),
            ));
        }
        let cells = [
            case.name.to_string(),
            converged_cell,
            count(outcome.as_ref().and_then(|outcome| outcome.sweeps)),
            count(outcome.as_ref().and_then(|outcome| outcome.evaluations)),
            if case.fit_metrics {
                sci(metrics.certificate)
            } else {
                "-".to_string()
            },
            if case.fit_metrics {
                sci(metrics.damping)
            } else {
                "-".to_string()
            },
            if case.fit_metrics {
                fixed(metrics.ratio)
            } else {
                "-".to_string()
            },
            if case.fit_metrics {
                failure_summary(&metrics.failures)
            } else {
                "-".to_string()
            },
            nuisance_summary(&metrics.nuisance),
            format!("{:.1}", run.wall.as_secs_f64()),
        ];
        Row {
            cells,
            converged,
            sweeps: if trace { metrics.sweeps } else { Vec::new() },
            detail,
        }
    }

    pub fn main() -> ExitCode {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.first().is_some_and(|arg| arg == "--case") {
            let Some(name) = args.get(1) else {
                eprintln!("fitbench: --case needs a name");
                return ExitCode::from(2);
            };
            let Some(case) = CASES.iter().find(|case| case.name == name) else {
                eprintln!("fitbench: unknown case {name}");
                return ExitCode::from(2);
            };
            let outcome = (case.run)();
            println!(
                "fitbench-result status=ok converged={} sweeps={} evaluations={} note={}",
                u8::from(outcome.converged),
                count(outcome.sweeps),
                count(outcome.evaluations),
                outcome.note.replace('\n', " "),
            );
            return ExitCode::SUCCESS;
        }
        let options = match parse_args(&args) {
            Ok(options) => options,
            Err(message) => {
                if !message.is_empty() {
                    eprintln!("fitbench: {message}");
                }
                usage();
                return ExitCode::from(2);
            }
        };
        if options.list {
            for case in CASES {
                let group = if case.slow { "slow" } else { "fast" };
                println!("{:<18} {:<5} {}", case.name, group, case.fixture);
            }
            return ExitCode::SUCCESS;
        }
        let cases = match select(&options) {
            Ok(cases) => cases,
            Err(message) => {
                eprintln!("fitbench: {message}");
                return ExitCode::from(2);
            }
        };
        if cases.is_empty() {
            println!("fitbench: no cases selected");
            return ExitCode::SUCCESS;
        }
        println!("fitbench: {} case(s)", cases.len());
        let started = Instant::now();
        let mut rows = Vec::with_capacity(cases.len());
        for case in &cases {
            let limit = options
                .max_seconds
                .map_or(Some(case.limit_seconds), |seconds| {
                    (seconds > 0.0).then_some(seconds)
                });
            let run = run_case(case, limit);
            rows.push(build_row(case, run, options.trace));
        }
        println!();
        print_table(&rows);
        if options.trace {
            print_trace(&rows);
        }
        let converged = rows.iter().filter(|row| row.converged).count();
        println!();
        println!(
            "fitbench: {converged}/{} converged in {:.1} s",
            rows.len(),
            started.elapsed().as_secs_f64()
        );
        if !options.all && !options.slow && options.names.is_empty() {
            let slow: Vec<&str> = CASES
                .iter()
                .filter(|case| case.slow)
                .map(|case| case.name)
                .collect();
            if !slow.is_empty() {
                println!(
                    "fitbench: slow group skipped ({}); --slow runs it",
                    slow.join(", ")
                );
            }
        }
        if converged == rows.len() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        }
    }
}

#[cfg(feature = "synth")]
fn main() -> std::process::ExitCode {
    bench::main()
}

#[cfg(not(feature = "synth"))]
fn main() -> std::process::ExitCode {
    eprintln!("fitbench requires --features synth");
    std::process::ExitCode::from(2)
}
