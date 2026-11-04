// SPDX-License-Identifier: AGPL-3.0-or-later
//! Plain string-building SVG plots: no JavaScript, fixed float formatting.
//!
//! Series carry colour, dash pattern and a direct label (docs/08 §7): the
//! Okabe-Ito palette is colour-blind safe, and no plot encodes a value by
//! colour alone.

use crate::compare::{
    DeltaEDistribution, GrlComponentKind, GrlPlot, GrlValue, HueBin, RegionMap, TintComparison,
    TintCurve, ToneComparison, ToneCurve,
};

/// Okabe-Ito blue.
pub const SERIES_A_COLOR: &str = "#0072B2";
/// Okabe-Ito vermillion.
pub const SERIES_B_COLOR: &str = "#D55E00";
const BAND_OPACITY: f64 = 0.16;

const TEXT_COLOR: &str = "#1a1a1a";
const MUTED_COLOR: &str = "#5a5a5a";
const GRID_COLOR: &str = "#e2e2e2";
const AXIS_COLOR: &str = "#4a4a4a";
const BACKGROUND: &str = "#ffffff";

/// Region-map ΔE band boundaries; the last band is open ended.
pub const DELTA_E_BANDS: [f64; 4] = [1.0, 2.0, 4.0, 8.0];
/// Fills for the ΔE bands, from light to dark.
pub const DELTA_E_FILLS: [&str; 5] = ["#F0E442", "#E69F00", "#D55E00", "#994400", "#5A2A00"];

#[derive(Clone, Copy, Debug)]
struct Style {
    color: &'static str,
    dash: &'static str,
}

const STYLE_A: Style = Style {
    color: SERIES_A_COLOR,
    dash: "none",
};

const STYLE_B: Style = Style {
    color: SERIES_B_COLOR,
    dash: "7 4",
};

type Pixels = Vec<(f64, f64)>;
type BandPixels = Vec<Option<(f64, f64, f64)>>;

#[derive(Clone)]
struct LineSeries {
    label: String,
    style: Style,
    points: Pixels,
    band: BandPixels,
}

impl LineSeries {
    fn new(label: impl Into<String>, style: Style, points: Vec<(f64, f64)>) -> Self {
        Self {
            label: label.into(),
            style,
            points,
            band: Vec::new(),
        }
    }
}

#[derive(Clone, Copy)]
struct Panel {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Panel {
    fn map_x(self, value: f64, min: f64, max: f64) -> f64 {
        if max <= min {
            self.x + self.width / 2.0
        } else {
            self.x + (value - min) / (max - min) * self.width
        }
    }

    fn map_y(self, value: f64, min: f64, max: f64) -> f64 {
        if max <= min {
            self.y + self.height / 2.0
        } else {
            self.y + self.height - (value - min) / (max - min) * self.height
        }
    }
}

pub(crate) fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(character),
        }
    }
    out
}

fn num(value: f64, decimals: usize) -> String {
    match decimals {
        0 => format!("{value:.0}"),
        1 => format!("{value:.1}"),
        2 => format!("{value:.2}"),
        _ => format!("{value:.3}"),
    }
}

fn open_svg(out: &mut String, prefix: &str, width: f64, height: f64, title: &str, desc: &str) {
    out.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-labelledby=\"{prefix}-title {prefix}-desc\">\n",
        w = num(width, 0),
        h = num(height, 0),
    ));
    out.push_str(&format!(
        "<title id=\"{prefix}-title\">{}</title>\n",
        escape(title)
    ));
    out.push_str(&format!(
        "<desc id=\"{prefix}-desc\">{}</desc>\n",
        escape(desc)
    ));
    out.push_str(&format!(
        "<rect width=\"{w}\" height=\"{h}\" fill=\"{BACKGROUND}\"/>\n",
        w = num(width, 0),
        h = num(height, 0),
    ));
}

fn close_svg(out: &mut String) {
    out.push_str("</svg>\n");
}

fn ticks(min: f64, max: f64, count: usize) -> Vec<f64> {
    if count == 0 {
        return Vec::new();
    }
    if count == 1 {
        return vec![min];
    }
    (0..count)
        .map(|index| min + (max - min) * index as f64 / (count - 1) as f64)
        .collect()
}

fn padded_domain(values: impl Iterator<Item = f64>, include_zero: bool) -> (f64, f64) {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for value in values {
        if value.is_finite() {
            min = min.min(value);
            max = max.max(value);
        }
    }
    if !min.is_finite() || !max.is_finite() {
        return (-1.0, 1.0);
    }
    if include_zero {
        min = min.min(0.0);
        max = max.max(0.0);
    }
    if max - min < 1e-6 {
        return (min - 0.5, max + 0.5);
    }
    let padding = (max - min) * 0.08;
    (min - padding, max + padding)
}

struct AxesSpec<'a> {
    x_domain: (f64, f64),
    y_domain: (f64, f64),
    x_ticks: Vec<f64>,
    y_ticks: Vec<f64>,
    x_label: &'a str,
    y_label: &'a str,
}

fn panel(out: &mut String, panel: Panel, axes: &AxesSpec<'_>, series: &[LineSeries]) {
    let (x_min, x_max) = axes.x_domain;
    let (y_min, y_max) = axes.y_domain;

    for tick in &axes.y_ticks {
        let y = panel.map_y(*tick, y_min, y_max);
        out.push_str(&format!(
            "<line x1=\"{x1}\" y1=\"{y}\" x2=\"{x2}\" y2=\"{y}\" stroke=\"{GRID_COLOR}\" stroke-width=\"1\"/>\n",
            x1 = num(panel.x, 1),
            y = num(y, 1),
            x2 = num(panel.x + panel.width, 1),
        ));
        out.push_str(&format!(
            "<text x=\"{x}\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"10\" fill=\"{MUTED_COLOR}\" text-anchor=\"end\" dominant-baseline=\"middle\">{}</text>\n",
            escape(&num(*tick, 1)),
            x = num(panel.x - 6.0, 1),
            y = num(y, 1),
        ));
    }
    for tick in &axes.x_ticks {
        let x = panel.map_x(*tick, x_min, x_max);
        out.push_str(&format!(
            "<text x=\"{x}\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"10\" fill=\"{MUTED_COLOR}\" text-anchor=\"middle\">{}</text>\n",
            escape(&num(*tick, 1)),
            x = num(x, 1),
            y = num(panel.y + panel.height + 14.0, 1),
        ));
    }

    out.push_str(&format!(
        "<line x1=\"{x1}\" y1=\"{y1}\" x2=\"{x1}\" y2=\"{y2}\" stroke=\"{AXIS_COLOR}\" stroke-width=\"1\"/>\n",
        x1 = num(panel.x, 1),
        y1 = num(panel.y, 1),
        y2 = num(panel.y + panel.height, 1),
    ));
    out.push_str(&format!(
        "<line x1=\"{x1}\" y1=\"{y2}\" x2=\"{x2}\" y2=\"{y2}\" stroke=\"{AXIS_COLOR}\" stroke-width=\"1\"/>\n",
        x1 = num(panel.x, 1),
        x2 = num(panel.x + panel.width, 1),
        y2 = num(panel.y + panel.height, 1),
    ));
    out.push_str(&format!(
        "<text x=\"{x}\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"11\" fill=\"{TEXT_COLOR}\" text-anchor=\"middle\">{}</text>\n",
        escape(axes.x_label),
        x = num(panel.x + panel.width / 2.0, 1),
        y = num(panel.y + panel.height + 30.0, 1),
    ));
    if !axes.y_label.is_empty() {
        out.push_str(&format!(
            "<text x=\"{x}\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"11\" fill=\"{TEXT_COLOR}\" text-anchor=\"middle\" transform=\"rotate(-90 {x} {y})\">{}</text>\n",
            escape(axes.y_label),
            x = num(panel.x - 40.0, 1),
            y = num(panel.y + panel.height / 2.0, 1),
        ));
    }

    for line in series {
        if let Some(path) = band_path(&line.band, panel, axes.x_domain, axes.y_domain) {
            out.push_str(&format!(
                "<path d=\"{}\" fill=\"{}\" fill-opacity=\"{}\" stroke=\"none\"/>\n",
                path,
                line.style.color,
                num(BAND_OPACITY, 2),
            ));
        }
        let points = map_points(&line.points, panel, axes.x_domain, axes.y_domain);
        out.push_str(&format!(
            "<polyline points=\"{}\" fill=\"none\" stroke=\"{}\" stroke-width=\"1.8\" stroke-dasharray=\"{}\" stroke-linejoin=\"round\"/>\n",
            points, line.style.color, line.style.dash,
        ));
        if let Some(last) = line.points.last() {
            let x = panel.map_x(last.0, x_min, x_max) + 6.0;
            let y = panel.map_y(last.1, y_min, y_max);
            out.push_str(&format!(
                "<text x=\"{x}\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"10\" fill=\"{}\" dominant-baseline=\"middle\">{}</text>\n",
                line.style.color,
                escape(&line.label),
                x = num(x, 1),
                y = num(y, 1),
            ));
        }
    }
}

fn map_points(
    points: &[(f64, f64)],
    panel: Panel,
    x_domain: (f64, f64),
    y_domain: (f64, f64),
) -> String {
    let mut out = String::new();
    for (index, (x, y)) in points.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(&format!(
            "{},{}",
            num(panel.map_x(*x, x_domain.0, x_domain.1), 1),
            num(panel.map_y(*y, y_domain.0, y_domain.1), 1)
        ));
    }
    out
}

fn band_path(
    band: &[Option<(f64, f64, f64)>],
    panel: Panel,
    x_domain: (f64, f64),
    y_domain: (f64, f64),
) -> Option<String> {
    let mut runs: Vec<Vec<(f64, f64, f64)>> = Vec::new();
    let mut current = Vec::new();
    for point in band {
        match point {
            Some(point) => current.push(*point),
            None => {
                if current.len() >= 2 {
                    runs.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
            }
        }
    }
    if current.len() >= 2 {
        runs.push(current);
    }
    if runs.is_empty() {
        return None;
    }
    let mut path = String::new();
    for run in runs {
        for (index, (x, _, high)) in run.iter().enumerate() {
            path.push_str(if index == 0 { "M " } else { " L " });
            path.push_str(&format!(
                "{},{}",
                num(panel.map_x(*x, x_domain.0, x_domain.1), 1),
                num(panel.map_y(*high, y_domain.0, y_domain.1), 1)
            ));
        }
        for (x, low, _) in run.iter().rev() {
            path.push_str(" L ");
            path.push_str(&format!(
                "{},{}",
                num(panel.map_x(*x, x_domain.0, x_domain.1), 1),
                num(panel.map_y(*low, y_domain.0, y_domain.1), 1)
            ));
        }
        path.push_str(" Z");
    }
    Some(path)
}

fn legend(out: &mut String, x: f64, y: f64, entries: &[(String, Style)]) {
    let mut cursor = x;
    for (label, style) in entries {
        out.push_str(&format!(
            "<line x1=\"{x1}\" y1=\"{y}\" x2=\"{x2}\" y2=\"{y}\" stroke=\"{}\" stroke-width=\"2\" stroke-dasharray=\"{}\"/>\n",
            style.color,
            style.dash,
            x1 = num(cursor, 1),
            x2 = num(cursor + 18.0, 1),
            y = num(y, 1),
        ));
        cursor += 24.0;
        out.push_str(&format!(
            "<text x=\"{x}\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"11\" fill=\"{TEXT_COLOR}\" dominant-baseline=\"middle\">{}</text>\n",
            escape(label),
            x = num(cursor, 1),
            y = num(y, 1),
        ));
        cursor += 10.0 + label.chars().count() as f64 * 6.2;
    }
}

fn chart_frame(
    title: &str,
    prefix: &str,
    width: f64,
    height: f64,
    series: &[LineSeries],
    body: impl FnOnce(&mut String),
) -> String {
    let mut out = String::new();
    open_svg(&mut out, prefix, width, height, title, title);
    let entries: Vec<(String, Style)> = series
        .iter()
        .map(|line| (line.label.clone(), line.style))
        .collect();
    if !entries.is_empty() {
        legend(&mut out, 56.0, 18.0, &entries);
    }
    body(&mut out);
    close_svg(&mut out);
    out
}

pub fn tone_curve_svg(curve: &ToneCurve) -> String {
    let points: Vec<(f64, f64)> = curve.points.iter().map(|p| (p.s, p.j)).collect();
    let y_domain = padded_domain(points.iter().map(|p| p.1), true);
    let series = vec![LineSeries::new("J′", STYLE_A, points)];
    chart_frame(
        "Tone curve: J′ against s",
        "tone-single",
        720.0,
        260.0,
        &series,
        |out| {
            panel(
                out,
                Panel {
                    x: 56.0,
                    y: 34.0,
                    width: 600.0,
                    height: 170.0,
                },
                &AxesSpec {
                    x_domain: (crate::compare::CURVE_MIN_S, crate::compare::CURVE_MAX_S),
                    y_domain,
                    x_ticks: ticks(crate::compare::CURVE_MIN_S, crate::compare::CURVE_MAX_S, 7),
                    y_ticks: ticks(y_domain.0, y_domain.1, 5),
                    x_label: "s (stops from 18% gray)",
                    y_label: "J′",
                },
                &series,
            );
        },
    )
}

/// Tone curves of both functions plus the difference.
pub fn tone_comparison_svg(plot: &ToneComparison) -> String {
    let a: Vec<(f64, f64)> = plot.points.iter().map(|p| (p.s, p.a)).collect();
    let b: Vec<(f64, f64)> = plot.points.iter().map(|p| (p.s, p.b)).collect();
    let differences: Vec<(f64, f64)> = plot.points.iter().map(|p| (p.s, p.delta)).collect();
    let top_domain = padded_domain(a.iter().chain(b.iter()).map(|p| p.1), true);
    let diff_domain = padded_domain(differences.iter().map(|p| p.1), true);
    let x_domain = (crate::compare::CURVE_MIN_S, crate::compare::CURVE_MAX_S);
    let series = vec![
        LineSeries::new("A", STYLE_A, a),
        LineSeries::new("B", STYLE_B, b),
    ];
    let diff_series = vec![LineSeries::new("B − A", STYLE_B, differences)];
    chart_frame(
        "Tone curves of both functions and their difference",
        "tone",
        720.0,
        400.0,
        &series,
        |out| {
            panel(
                out,
                Panel {
                    x: 56.0,
                    y: 34.0,
                    width: 600.0,
                    height: 150.0,
                },
                &AxesSpec {
                    x_domain,
                    y_domain: top_domain,
                    x_ticks: ticks(x_domain.0, x_domain.1, 7),
                    y_ticks: ticks(top_domain.0, top_domain.1, 5),
                    x_label: "s (stops from 18% gray)",
                    y_label: "J′",
                },
                &series,
            );
            panel(
                out,
                Panel {
                    x: 56.0,
                    y: 236.0,
                    width: 600.0,
                    height: 110.0,
                },
                &AxesSpec {
                    x_domain,
                    y_domain: diff_domain,
                    x_ticks: ticks(x_domain.0, x_domain.1, 7),
                    y_ticks: ticks(diff_domain.0, diff_domain.1, 3),
                    x_label: "s (stops from 18% gray)",
                    y_label: "B − A (J′)",
                },
                &diff_series,
            );
        },
    )
}

fn tint_panels(
    title: &str,
    prefix: &str,
    a: Option<&TintCurve>,
    comparison: Option<&TintComparison>,
) -> String {
    let (points_a, points_b): (Pixels, Pixels) = match (a, comparison) {
        (Some(curve), _) => (
            curve.points.iter().map(|p| (p.j, p.a)).collect(),
            Vec::new(),
        ),
        (None, Some(plot)) => (
            plot.points.iter().map(|p| (p.j_a, p.a_a)).collect(),
            plot.points.iter().map(|p| (p.j_b, p.a_b)).collect(),
        ),
        (None, None) => (Vec::new(), Vec::new()),
    };
    let (points_a_b, points_b_b): (Pixels, Pixels) = match (a, comparison) {
        (Some(curve), _) => (
            curve.points.iter().map(|p| (p.j, p.b)).collect(),
            Vec::new(),
        ),
        (None, Some(plot)) => (
            plot.points.iter().map(|p| (p.j_a, p.b_a)).collect(),
            plot.points.iter().map(|p| (p.j_b, p.b_b)).collect(),
        ),
        (None, None) => (Vec::new(), Vec::new()),
    };

    let single = comparison.is_none();
    let label_a = if single { "a′" } else { "A" };
    let label_b = if single { "b′" } else { "B" };
    let mut series = vec![LineSeries::new(label_a, STYLE_A, points_a.clone())];
    if !points_b.is_empty() {
        series.push(LineSeries::new(label_b, STYLE_B, points_b.clone()));
    }
    let mut series_b = vec![LineSeries::new(label_a, STYLE_A, points_a_b.clone())];
    if !points_b_b.is_empty() {
        series_b.push(LineSeries::new(label_b, STYLE_B, points_b_b.clone()));
    }

    let mut all = series.clone();
    all.extend(series_b.iter().map(|line| LineSeries {
        label: line.label.clone(),
        style: line.style,
        points: line.points.clone(),
        band: Vec::new(),
    }));

    let x_domain = padded_domain(
        points_a
            .iter()
            .chain(points_b.iter())
            .chain(points_a_b.iter())
            .chain(points_b_b.iter())
            .map(|p| p.0),
        false,
    );
    let a_domain = padded_domain(points_a.iter().chain(points_b.iter()).map(|p| p.1), true);
    let b_domain = padded_domain(
        points_a_b.iter().chain(points_b_b.iter()).map(|p| p.1),
        true,
    );

    let mut out = String::new();
    open_svg(
        &mut out,
        prefix,
        720.0,
        420.0,
        title,
        "Neutral tint: a′ and b′ against J′",
    );
    legend(
        &mut out,
        56.0,
        18.0,
        &all.iter()
            .map(|line| (line.label.clone(), line.style))
            .collect::<Vec<_>>(),
    );
    panel(
        &mut out,
        Panel {
            x: 56.0,
            y: 34.0,
            width: 600.0,
            height: 150.0,
        },
        &AxesSpec {
            x_domain,
            y_domain: a_domain,
            x_ticks: ticks(x_domain.0, x_domain.1, 6),
            y_ticks: ticks(a_domain.0, a_domain.1, 5),
            x_label: "J′",
            y_label: "a′",
        },
        &series,
    );
    panel(
        &mut out,
        Panel {
            x: 56.0,
            y: 240.0,
            width: 600.0,
            height: 150.0,
        },
        &AxesSpec {
            x_domain,
            y_domain: b_domain,
            x_ticks: ticks(x_domain.0, x_domain.1, 6),
            y_ticks: ticks(b_domain.0, b_domain.1, 5),
            x_label: "J′",
            y_label: "b′",
        },
        &series_b,
    );
    close_svg(&mut out);
    out
}

pub fn tint_curve_svg(curve: &TintCurve) -> String {
    tint_panels(
        "Neutral tint: a′ and b′ against J′",
        "tint-single",
        Some(curve),
        None,
    )
}

pub fn tint_comparison_svg(plot: &TintComparison) -> String {
    tint_panels("Neutral tint of both functions", "tint", None, Some(plot))
}

fn grl_series_points(values: &[Option<GrlValue>], bins: &[HueBin]) -> (Pixels, BandPixels) {
    let mut points = Vec::with_capacity(values.len());
    let mut band = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let Some(value) = value else {
            band.push(None);
            continue;
        };
        let Some(bin) = bins.get(index) else {
            continue;
        };
        let center = (bin.start_deg + bin.end_deg) / 2.0;
        points.push((center, value.value));
        band.push(
            value
                .sigma
                .map(|sigma| (center, value.value - sigma, value.value + sigma)),
        );
    }
    (points, band)
}

/// Chroma, hue and lightness layers by hue, small multiples over `s`.
pub fn grl_svg(plot: &GrlPlot) -> String {
    let panel_width = 190.0;
    let panel_height = 110.0;
    let left = 64.0;
    let top = 54.0;
    let gap_x = 18.0;
    let gap_y = 46.0;
    let columns = plot.s_values.len();
    let width = left + columns as f64 * (panel_width + gap_x) + 20.0;
    let height = top + 3.0 * (panel_height + gap_y) + 20.0;

    let has_b = plot.series.iter().any(|series| !series.b.is_empty());
    let labels: Vec<(String, Style)> = if has_b {
        vec![("A".to_string(), STYLE_A), ("B".to_string(), STYLE_B)]
    } else {
        vec![("A".to_string(), STYLE_A)]
    };
    let mut out = String::new();
    open_svg(
        &mut out,
        "grl",
        width,
        height,
        "Chroma, hue and lightness by hue",
        "Chroma gain, hue rotation and lightness shift against hue for each lightness",
    );
    legend(&mut out, left, 20.0, &labels);

    for (row, component) in GrlComponentKind::ALL.iter().enumerate() {
        let row_y = top + row as f64 * (panel_height + gap_y);
        out.push_str(&format!(
            "<text x=\"{x}\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"11\" fill=\"{TEXT_COLOR}\" text-anchor=\"middle\" transform=\"rotate(-90 {x} {y})\">{}</text>\n",
            escape(component.name()),
            x = num(26.0, 1),
            y = num(row_y + panel_height / 2.0, 1),
        ));
        for (column, s) in plot.s_values.iter().enumerate() {
            let column_x = left + column as f64 * (panel_width + gap_x);
            out.push_str(&format!(
                "<text x=\"{x}\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"10\" fill=\"{MUTED_COLOR}\" text-anchor=\"middle\">s = {}</text>\n",
                escape(&num(*s, 1)),
                x = num(column_x + panel_width / 2.0, 1),
                y = num(row_y - 6.0, 1),
            ));
            let Some(series) = plot
                .series
                .iter()
                .find(|series| series.component == *component && series.s == *s)
            else {
                continue;
            };
            let (points_a, band_a) = grl_series_points(&series.a, &plot.hue_bins);
            let series_a = LineSeries {
                label: "A".to_string(),
                style: STYLE_A,
                points: points_a,
                band: band_a,
            };
            let mut curves = vec![series_a];
            if !series.b.is_empty() {
                let (points_b, band_b) = grl_series_points(&series.b, &plot.hue_bins);
                curves.push(LineSeries {
                    label: "B".to_string(),
                    style: STYLE_B,
                    points: points_b,
                    band: band_b,
                });
            }
            let domain = padded_domain(
                curves
                    .iter()
                    .flat_map(|line| line.points.iter().map(|p| p.1))
                    .chain(std::iter::once(0.0)),
                true,
            );
            let ticks_x = [0.0, 90.0, 180.0, 270.0, 360.0];
            panel(
                &mut out,
                Panel {
                    x: column_x,
                    y: row_y,
                    width: panel_width,
                    height: panel_height,
                },
                &AxesSpec {
                    x_domain: (0.0, 360.0),
                    y_domain: domain,
                    x_ticks: ticks_x.to_vec(),
                    y_ticks: vec![domain.0, (domain.0 + domain.1) / 2.0, domain.1],
                    x_label: "hue (deg)",
                    y_label: "",
                },
                &curves,
            );
        }
    }
    close_svg(&mut out);
    out
}

/// Histogram of a ΔE distribution, with median and p95 markers.
pub fn delta_e_histogram_svg(dist: &DeltaEDistribution) -> String {
    let width = 720.0;
    let height = 280.0;
    let panel_box = Panel {
        x: 56.0,
        y: 30.0,
        width: 620.0,
        height: 180.0,
    };
    let prefix = match dist.metric {
        crate::compare::DistributionMetric::DeltaEUcs => "histogram-ucs",
        crate::compare::DistributionMetric::Ciede2000 => "histogram-ciede2000",
    };
    let mut out = String::new();
    open_svg(
        &mut out,
        prefix,
        width,
        height,
        &format!("{} histogram", dist.metric.label()),
        "Distribution of the colour difference over supported probes",
    );
    out.push_str(&format!(
        "<text x=\"{x}\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"11\" fill=\"{TEXT_COLOR}\" text-anchor=\"middle\">{} over {} supported probes</text>\n",
        escape(dist.metric.label()),
        dist.count,
        x = num(width / 2.0, 1),
        y = num(16.0, 1),
    ));
    let max_count = dist
        .histogram
        .iter()
        .map(|bin| bin.count)
        .max()
        .unwrap_or(0);
    let x_max = dist
        .histogram
        .last()
        .map(|bin| bin.end)
        .unwrap_or(1.0)
        .max(1e-6);
    for bin in &dist.histogram {
        let x0 = panel_box.map_x(bin.start, 0.0, x_max);
        let x1 = panel_box.map_x(bin.end, 0.0, x_max);
        let y = panel_box.map_y(bin.count as f64, 0.0, max_count.max(1) as f64);
        let bar_width = (x1 - x0 - 1.0).max(1.0);
        out.push_str(&format!(
            "<rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" fill=\"{SERIES_A_COLOR}\" fill-opacity=\"0.55\"/>\n",
            x = num(x0, 1),
            y = num(y, 1),
            w = num(bar_width, 1),
            h = num(panel_box.y + panel_box.height - y, 1),
        ));
    }
    for (fraction, label) in [
        (0.0, "0".to_string()),
        (0.5, String::new()),
        (1.0, num(x_max, 1)),
    ] {
        out.push_str(&format!(
            "<text x=\"{x}\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"10\" fill=\"{MUTED_COLOR}\" text-anchor=\"middle\">{}</text>\n",
            escape(&label),
            x = num(panel_box.x + panel_box.width * fraction, 1),
            y = num(panel_box.y + panel_box.height + 16.0, 1),
        ));
    }
    out.push_str(&format!(
        "<line x1=\"{x1}\" y1=\"{y1}\" x2=\"{x1}\" y2=\"{y2}\" stroke=\"{AXIS_COLOR}\" stroke-width=\"1\"/>\n",
        x1 = num(panel_box.x, 1),
        y1 = num(panel_box.y, 1),
        y2 = num(panel_box.y + panel_box.height, 1),
    ));
    out.push_str(&format!(
        "<line x1=\"{x1}\" y1=\"{y2}\" x2=\"{x2}\" y2=\"{y2}\" stroke=\"{AXIS_COLOR}\" stroke-width=\"1\"/>\n",
        x1 = num(panel_box.x, 1),
        x2 = num(panel_box.x + panel_box.width, 1),
        y2 = num(panel_box.y + panel_box.height, 1),
    ));
    for (value, label, color) in [
        (dist.median, "median", MUTED_COLOR),
        (dist.p95, "p95", SERIES_B_COLOR),
    ] {
        let x = panel_box.map_x(value, 0.0, x_max);
        out.push_str(&format!(
            "<line x1=\"{x}\" y1=\"{y1}\" x2=\"{x}\" y2=\"{y2}\" stroke=\"{color}\" stroke-width=\"1.2\" stroke-dasharray=\"4 3\"/>\n",
            x = num(x, 1),
            y1 = num(panel_box.y, 1),
            y2 = num(panel_box.y + panel_box.height, 1),
        ));
        out.push_str(&format!(
            "<text x=\"{x}\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"10\" fill=\"{color}\" text-anchor=\"middle\">{} {}</text>\n",
            escape(label),
            escape(&num(value, 2)),
            x = num(x, 1),
            y = num(panel_box.y - 6.0, 1),
        ));
    }
    out.push_str(&format!(
        "<text x=\"{x}\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"11\" fill=\"{TEXT_COLOR}\" text-anchor=\"middle\">ΔE</text>\n",
        x = num(panel_box.x + panel_box.width / 2.0, 1),
        y = num(panel_box.y + panel_box.height + 30.0, 1),
    ));
    close_svg(&mut out);
    out
}

fn band_index(delta: f64) -> usize {
    let mut index = DELTA_E_BANDS.len();
    for (position, bound) in DELTA_E_BANDS.iter().enumerate() {
        if delta < *bound {
            index = position;
            break;
        }
    }
    index
}

/// Region map of ΔE over `(hue, s)`; unsupported cells are hatched.
pub fn region_map_svg(map: &RegionMap) -> String {
    let cell_width = 30.0;
    let cell_height = 22.0;
    let left = 56.0;
    let top = 48.0;
    let width = left + map.hue_bins.len() as f64 * cell_width + 24.0;
    let height = top + map.s_values.len() as f64 * cell_height + 96.0;

    let mut out = String::new();
    open_svg(
        &mut out,
        "region",
        width,
        height,
        "ΔE region map",
        "Colour difference over hue and lightness; unsupported cells are hatched",
    );
    out.push_str("<defs>\n");
    out.push_str(
        "<pattern id=\"hatch-unsupported\" width=\"6\" height=\"6\" patternUnits=\"userSpaceOnUse\" patternTransform=\"rotate(45)\">\n",
    );
    out.push_str("<rect width=\"6\" height=\"6\" fill=\"#f2f2f2\"/>\n");
    out.push_str(
        "<line x1=\"0\" y1=\"0\" x2=\"0\" y2=\"6\" stroke=\"#8a8a8a\" stroke-width=\"1.4\"/>\n",
    );
    out.push_str("</pattern>\n</defs>\n");

    for (row, s) in map.s_values.iter().enumerate() {
        out.push_str(&format!(
            "<text x=\"{x}\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"10\" fill=\"{MUTED_COLOR}\" text-anchor=\"end\">{}</text>\n",
            escape(&num(*s, 1)),
            x = num(left - 6.0, 1),
            y = num(top + row as f64 * cell_height + cell_height / 2.0 + 3.0, 1),
        ));
    }
    out.push_str(&format!(
        "<text x=\"{x}\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"11\" fill=\"{TEXT_COLOR}\" text-anchor=\"middle\" transform=\"rotate(-90 {x} {y})\">s</text>\n",
        x = num(18.0, 1),
        y = num(top + map.s_values.len() as f64 * cell_height / 2.0, 1),
    ));

    for (column, bin) in map.hue_bins.iter().enumerate() {
        if bin.index % 3 == 0 {
            out.push_str(&format!(
                "<text x=\"{x}\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"10\" fill=\"{MUTED_COLOR}\" text-anchor=\"middle\">{}</text>\n",
                escape(&num(bin.start_deg, 0)),
                x = num(left + column as f64 * cell_width + cell_width / 2.0, 1),
                y = num(top - 8.0, 1),
            ));
        }
    }
    out.push_str(&format!(
        "<text x=\"{x}\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"11\" fill=\"{TEXT_COLOR}\" text-anchor=\"middle\">hue (deg)</text>\n",
        x = num(left + map.hue_bins.len() as f64 * cell_width / 2.0, 1),
        y = num(top - 26.0, 1),
    ));

    for (column, _) in map.hue_bins.iter().enumerate() {
        for (row, _) in map.s_values.iter().enumerate() {
            let Some(cell) = map.cells.get(column).and_then(|cells| cells.get(row)) else {
                continue;
            };
            let x = left + column as f64 * cell_width;
            let y = top + row as f64 * cell_height;
            match cell.delta_e {
                Some(delta) => {
                    let fill = DELTA_E_FILLS[band_index(delta)];
                    let text = if delta >= 10.0 {
                        num(delta, 0)
                    } else {
                        num(delta, 1)
                    };
                    let text_fill = if band_index(delta) >= 2 {
                        "#ffffff"
                    } else {
                        TEXT_COLOR
                    };
                    let tooltip = if cell.sigma > 0.0 {
                        format!(
                            "{}: ΔE {} ± {}",
                            cell.probe.as_ref().map_or("", |id| id.as_str()),
                            num(delta, 2),
                            num(cell.sigma, 2)
                        )
                    } else {
                        format!(
                            "{}: ΔE {}",
                            cell.probe.as_ref().map_or("", |id| id.as_str()),
                            num(delta, 2)
                        )
                    };
                    out.push_str(&format!(
                        "<g><title>{}</title><rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" fill=\"{fill}\" stroke=\"#ffffff\" stroke-width=\"1\"/><text x=\"{tx}\" y=\"{ty}\" font-family=\"sans-serif\" font-size=\"9\" fill=\"{text_fill}\" text-anchor=\"middle\" dominant-baseline=\"middle\">{}</text></g>\n",
                        escape(&tooltip),
                        escape(&text),
                        x = num(x, 1),
                        y = num(y, 1),
                        w = num(cell_width - 2.0, 1),
                        h = num(cell_height - 2.0, 1),
                        tx = num(x + cell_width / 2.0 - 1.0, 1),
                        ty = num(y + cell_height / 2.0, 1),
                    ));
                }
                None => {
                    out.push_str(&format!(
                        "<g><title>unsupported</title><rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" fill=\"url(#hatch-unsupported)\" stroke=\"#ffffff\" stroke-width=\"1\"/></g>\n",
                        x = num(x, 1),
                        y = num(y, 1),
                        w = num(cell_width - 2.0, 1),
                        h = num(cell_height - 2.0, 1),
                    ));
                }
            }
        }
    }

    let legend_y = top + map.s_values.len() as f64 * cell_height + 22.0;
    out.push_str(&format!(
        "<rect x=\"{x}\" y=\"{y}\" width=\"18\" height=\"12\" fill=\"url(#hatch-unsupported)\" stroke=\"#8a8a8a\"/>\n",
        x = num(left, 1),
        y = num(legend_y, 1),
    ));
    out.push_str(&format!(
        "<text x=\"{x}\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"10\" fill=\"{MUTED_COLOR}\">unsupported (hatched)</text>\n",
        x = num(left + 24.0, 1),
        y = num(legend_y + 10.0, 1),
    ));
    let mut cursor = left + 170.0;
    for (index, fill) in DELTA_E_FILLS.iter().enumerate() {
        let label = match index {
            0 => format!("< {}", num(DELTA_E_BANDS[0], 0)),
            1 => format!("{}-{}", num(DELTA_E_BANDS[0], 0), num(DELTA_E_BANDS[1], 0)),
            2 => format!("{}-{}", num(DELTA_E_BANDS[1], 0), num(DELTA_E_BANDS[2], 0)),
            3 => format!("{}-{}", num(DELTA_E_BANDS[2], 0), num(DELTA_E_BANDS[3], 0)),
            _ => format!("≥ {}", num(DELTA_E_BANDS[3], 0)),
        };
        out.push_str(&format!(
            "<rect x=\"{x}\" y=\"{y}\" width=\"18\" height=\"12\" fill=\"{fill}\"/>\n",
            x = num(cursor, 1),
            y = num(legend_y, 1),
        ));
        out.push_str(&format!(
            "<text x=\"{x}\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"10\" fill=\"{MUTED_COLOR}\">{}</text>\n",
            escape(&label),
            x = num(cursor + 22.0, 1),
            y = num(legend_y + 10.0, 1),
        ));
        cursor += 70.0;
    }
    out.push_str(&format!(
        "<text x=\"{x}\" y=\"{y}\" font-family=\"sans-serif\" font-size=\"11\" fill=\"{TEXT_COLOR}\" text-anchor=\"middle\">ΔE</text>\n",
        x = num(width / 2.0, 1),
        y = num(legend_y + 34.0, 1),
    ));
    close_svg(&mut out);
    out
}
