// SPDX-License-Identifier: AGPL-3.0-or-later
//! One-line natural-language comparison summary (docs/02 §7 item 8).

use serde::Serialize;

use crate::compare::{
    DeltaEDistribution, FunctionName, GrlComponentKind, GrlPlot, GrlSeries, GrlValue,
    LayerContribution, LayerId, NO_DIFFERENCE_DE, TintComparison, ToneComparison,
};

/// Named hue regions, grouped from the 24-bin hue grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HueRegion {
    Reds,
    Oranges,
    Yellows,
    Greens,
    Cyans,
    Blues,
    Purples,
    Magentas,
}

impl HueRegion {
    pub const ALL: [Self; 8] = [
        Self::Reds,
        Self::Oranges,
        Self::Yellows,
        Self::Greens,
        Self::Cyans,
        Self::Blues,
        Self::Purples,
        Self::Magentas,
    ];

    /// Inclusive lower and exclusive upper hue bounds in degrees.
    pub fn bounds(self) -> (f64, f64) {
        match self {
            Self::Reds => (0.0, 40.0),
            Self::Oranges => (40.0, 75.0),
            Self::Yellows => (75.0, 115.0),
            Self::Greens => (115.0, 165.0),
            Self::Cyans => (165.0, 205.0),
            Self::Blues => (205.0, 265.0),
            Self::Purples => (265.0, 310.0),
            Self::Magentas => (310.0, 360.0),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Reds => "reds",
            Self::Oranges => "oranges",
            Self::Yellows => "yellows",
            Self::Greens => "greens",
            Self::Cyans => "cyans",
            Self::Blues => "blues",
            Self::Purples => "purples",
            Self::Magentas => "magentas",
        }
    }

    pub fn range_label(self) -> String {
        let (start, end) = self.bounds();
        format!("{start:.0}–{end:.0}°")
    }

    pub fn of_degrees(degrees: f64) -> Self {
        let mut hue = degrees % 360.0;
        if hue < 0.0 {
            hue += 360.0;
        }
        for region in Self::ALL {
            let (start, end) = region.bounds();
            if hue >= start && hue < end {
                return region;
            }
        }
        Self::Magentas
    }

    pub fn of_bin(index: usize) -> Self {
        Self::of_degrees(index as f64 * 15.0 + 7.5)
    }
}

pub fn summary_line(
    a: &FunctionName,
    b: &FunctionName,
    tone: &ToneComparison,
    tint: &TintComparison,
    grl: Option<&GrlPlot>,
    delta_e: &DeltaEDistribution,
    attribution: &[LayerContribution],
) -> String {
    if delta_e.max <= NO_DIFFERENCE_DE {
        return format!(
            "{b} is indistinguishable from {a}: no meaningful difference on the probe set."
        );
    }

    let mut phrases = Vec::new();
    for contribution in attribution {
        if contribution.contribution <= 0.001 {
            continue;
        }
        if let Some(phrase) = phrase(contribution, tone, tint, grl) {
            phrases.push(phrase);
        }
        if phrases.len() == 3 {
            break;
        }
    }

    if phrases.is_empty() {
        return format!(
            "{b} differs from {a} on the probe set: median ΔE {:.2}, p95 {:.2}, max {:.2}.",
            delta_e.median, delta_e.p95, delta_e.max
        );
    }
    format!("{b} differs from {a}: {}.", join(&phrases))
}

fn join(phrases: &[String]) -> String {
    match phrases {
        [] => String::new(),
        [only] => only.clone(),
        [first, second] => format!("{first} and {second}"),
        [first, second, third] => format!("{first}, {second}, and {third}"),
        [first, second, rest @ ..] => {
            let mut out = format!("{first}, {second}");
            for phrase in rest {
                out.push_str(", ");
                out.push_str(phrase);
            }
            out
        }
    }
}

fn phrase(
    contribution: &LayerContribution,
    tone: &ToneComparison,
    tint: &TintComparison,
    grl: Option<&GrlPlot>,
) -> Option<String> {
    match contribution.layer {
        LayerId::Tone => tone_phrase(tone),
        LayerId::Tint => tint_phrase(tint),
        LayerId::Chroma => chroma_phrase(grl?),
        LayerId::Hue => hue_phrase(grl?),
        LayerId::Lightness => lightness_phrase(grl?),
        LayerId::Residual => Some(format!(
            "a residual correction worth {:.2} mean ΔE",
            contribution.contribution
        )),
    }
}

fn mean_delta(tone: &ToneComparison, keep: impl Fn(f64) -> bool) -> Option<f64> {
    let mut sum = 0.0;
    let mut count = 0_usize;
    for sample in &tone.points {
        if keep(sample.s) {
            sum += sample.delta;
            count += 1;
        }
    }
    if count == 0 {
        None
    } else {
        Some(sum / count as f64)
    }
}

fn tone_phrase(tone: &ToneComparison) -> Option<String> {
    let candidates = [
        (
            mean_delta(tone, |s| s >= 1.0),
            "the highlights",
            "above s = +1",
        ),
        (
            mean_delta(tone, |s| s <= -3.0),
            "the shadows",
            "below s = -3",
        ),
        (
            mean_delta(tone, |s| (-2.0..0.0).contains(&s)),
            "the midtones",
            "around s = -1",
        ),
    ];
    let best = candidates
        .into_iter()
        .filter_map(|(delta, name, position)| delta.map(|delta| (delta, name, position)))
        .max_by(|left, right| left.0.abs().total_cmp(&right.0.abs()))?;
    let (delta, name, position) = best;
    if delta.abs() < crate::compare::TONE_EFFECT_FLOOR {
        return None;
    }
    let direction = if delta > 0.0 { "brighter" } else { "darker" };
    Some(format!("{direction} in {name} ({delta:+.1} J′ {position})"))
}

fn tint_phrase(tint: &TintComparison) -> Option<String> {
    if tint.points.is_empty() {
        return None;
    }
    let count = tint.points.len() as f64;
    let delta_a = tint.points.iter().map(|p| p.delta_a).sum::<f64>() / count;
    let delta_b = tint.points.iter().map(|p| p.delta_b).sum::<f64>() / count;
    let floor = crate::compare::TINT_EFFECT_FLOOR;
    let redder = delta_a.abs() >= floor;
    let warmer = delta_b.abs() >= floor;
    let a_word = if delta_a > 0.0 { "redder" } else { "greener" };
    let b_word = if delta_b > 0.0 { "warmer" } else { "cooler" };
    match (redder, warmer) {
        (true, true) => Some(format!(
            "{b_word} and {a_word} neutrals (a′ {delta_a:+.1}, b′ {delta_b:+.1})"
        )),
        (true, false) => Some(format!("{a_word} neutrals (a′ {delta_a:+.1})")),
        (false, true) => Some(format!("{b_word} neutrals (b′ {delta_b:+.1})")),
        (false, false) => None,
    }
}

fn series_at(grl: &GrlPlot, component: GrlComponentKind) -> Option<&GrlSeries> {
    grl.series
        .iter()
        .find(|series| series.component == component && series.s == 0.0)
}

fn dominant(first: &[Option<GrlValue>], second: &[Option<GrlValue>]) -> Option<(f64, usize)> {
    let mut best: Option<(f64, usize)> = None;
    for (bin, (first, second)) in first.iter().zip(second.iter()).enumerate() {
        let (Some(first), Some(second)) = (first, second) else {
            continue;
        };
        let delta = second.value - first.value;
        match best {
            None => best = Some((delta, bin)),
            Some((current, _)) if delta.abs() > current.abs() => best = Some((delta, bin)),
            _ => {}
        }
    }
    best
}

fn chroma_phrase(grl: &GrlPlot) -> Option<String> {
    let series = series_at(grl, GrlComponentKind::ChromaGain)?;
    let (delta, bin) = dominant(&series.a, &series.b)?;
    if delta.abs() < crate::compare::CHROMA_EFFECT_FLOOR {
        return None;
    }
    let percent = (libm::exp(delta) - 1.0) * 100.0;
    let direction = if percent > 0.0 { "more" } else { "less" };
    let region = grl.hue_bins.get(bin)?.region;
    Some(format!(
        "{direction} saturated in {} ({percent:+.0}% chroma, {})",
        region.name(),
        region.range_label()
    ))
}

fn hue_phrase(grl: &GrlPlot) -> Option<String> {
    let series = series_at(grl, GrlComponentKind::HueRotation)?;
    let (delta, bin) = dominant(&series.a, &series.b)?;
    if delta.abs() < crate::compare::HUE_EFFECT_FLOOR {
        return None;
    }
    let region = grl.hue_bins.get(bin)?.region;
    Some(format!(
        "hue rotated by {delta:+.0}° in {} ({})",
        region.name(),
        region.range_label()
    ))
}

fn lightness_phrase(grl: &GrlPlot) -> Option<String> {
    let series = series_at(grl, GrlComponentKind::LightnessShift)?;
    let (delta, bin) = dominant(&series.a, &series.b)?;
    if delta.abs() < crate::compare::LIGHTNESS_EFFECT_FLOOR {
        return None;
    }
    let direction = if delta > 0.0 { "lighter" } else { "darker" };
    let region = grl.hue_bins.get(bin)?.region;
    Some(format!(
        "{direction} in {} ({delta:+.1} J′, {})",
        region.name(),
        region.range_label()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hue_regions_cover_the_circle() {
        assert_eq!(HueRegion::of_degrees(0.0), HueRegion::Reds);
        assert_eq!(HueRegion::of_degrees(359.0), HueRegion::Magentas);
        assert_eq!(HueRegion::of_degrees(240.0), HueRegion::Blues);
        assert_eq!(HueRegion::of_degrees(96.0), HueRegion::Yellows);
        for region in HueRegion::ALL {
            let (start, _) = region.bounds();
            assert_eq!(HueRegion::of_degrees(start), region);
        }
    }

    #[test]
    fn joins_phrases_with_a_serial_comma() {
        let phrases = ["one".to_string(), "two".to_string(), "three".to_string()];
        assert_eq!(join(&phrases), "one, two, and three");
    }
}
