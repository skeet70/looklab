// SPDX-License-Identifier: AGPL-3.0-or-later
//! Self-contained HTML report pages for a Look and for a comparison.

use looklab_model::LookDocument;
use looklab_model::file::{BodyProvenance, LookIdentity, LookKind, Provenance, WbPolicy};

use crate::ArtifactHash;
use crate::compare::{
    Comparison, DeltaEDistribution, LookDiagnostics, RegionMap, ToneComparison, ToneCurve,
};
use crate::svg::{self, escape};

const CSS: &str = include_str!("report.css");

/// Identity, provenance and artifact hash of one reported Look.
#[derive(Clone, Debug, Default)]
pub struct ReportSource<'a> {
    pub label: String,
    pub identity: Option<&'a LookIdentity>,
    pub provenance: Option<&'a Provenance>,
    pub artifact_hash: Option<&'a ArtifactHash>,
    /// Whether the artifact hash was checked against a version manifest.
    pub manifest_matches: Option<bool>,
}

impl<'a> ReportSource<'a> {
    pub fn from_document(document: &'a LookDocument) -> Self {
        let identity = &document.file.look;
        Self {
            label: format!("{} {}", identity.slug.as_str(), identity.version.as_str()),
            identity: Some(identity),
            provenance: Some(&document.file.provenance),
            artifact_hash: None,
            manifest_matches: None,
        }
    }
}

fn page(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n<style>\n{CSS}</style>\n</head>\n<body>\n<main>\n{}\n</main>\n</body>\n</html>\n",
        escape(title),
        body
    )
}

fn kind_label(kind: LookKind) -> &'static str {
    match kind {
        LookKind::NativeFamily => "native family",
        LookKind::LutDefined => "LUT-defined",
    }
}

fn wb_label(policy: WbPolicy) -> &'static str {
    match policy {
        WbPolicy::Adaptive => "adaptive",
        WbPolicy::FixedIlluminant => "fixed illuminant",
    }
}

fn row(label: &str, value: &str) -> String {
    format!(
        "<tr><th scope=\"row\">{}</th><td>{}</td></tr>\n",
        escape(label),
        escape(value)
    )
}

fn rows(label: &str, values: &[String]) -> String {
    let joined = values.join(", ");
    row(label, &joined)
}

fn number(value: f64, decimals: usize) -> String {
    match decimals {
        0 => format!("{value:.0}"),
        1 => format!("{value:.1}"),
        _ => format!("{value:.2}"),
    }
}

fn identity_table(identity: &LookIdentity) -> String {
    let mut out = String::from("<table class=\"kv\">\n");
    out.push_str(&row("Look", identity.slug.as_str()));
    out.push_str(&row("Version", identity.version.as_str()));
    out.push_str(&row("Kind", kind_label(identity.kind)));
    out.push_str(&row("WB policy", wb_label(identity.wb_policy)));
    out.push_str("</table>\n");
    out
}

fn body_table(bodies: &[BodyProvenance]) -> String {
    if bodies.is_empty() {
        return "<p class=\"muted\">No bodies recorded.</p>\n".to_string();
    }
    let mut out = String::from(
        "<table>\n<thead><tr><th>Body</th><th>Epochs</th><th>Images</th><th>Credentials</th>\
         <th>Clip k</th><th>Transform</th><th>Realization</th><th>Response</th></tr></thead>\n<tbody>\n",
    );
    for body in bodies {
        let epochs: Vec<String> = body.epochs.iter().map(|epoch| epoch.to_string()).collect();
        out.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
            escape(&body.body),
            escape(&epochs.join(", ")),
            body.images,
            body.credentials,
            escape(&number(body.clip_k, 3)),
            body.input_transform_version,
            body.realization_params_version,
            body.control_response_version,
        ));
    }
    out.push_str("</tbody>\n</table>\n");
    out
}

fn artifact_block(source: &ReportSource<'_>) -> String {
    let Some(hash) = source.artifact_hash else {
        return "<p class=\"muted\">No artifact hash recorded.</p>\n".to_string();
    };
    let indicator = match source.manifest_matches {
        Some(true) => "<span class=\"ok\">matches version manifest</span>",
        Some(false) => "<span class=\"bad\">does not match version manifest</span>",
        None => "<span class=\"muted\">manifest not checked</span>",
    };
    format!(
        "<p class=\"hash\"><code>sha256:{}</code> {indicator}</p>\n",
        escape(hash.as_str())
    )
}

fn source_section(source: &ReportSource<'_>) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "<section>\n<h2>{}</h2>\n",
        escape(if source.label.is_empty() {
            "Source"
        } else {
            &source.label
        })
    ));
    if let Some(identity) = source.identity {
        out.push_str(&identity_table(identity));
    }
    out.push_str("<h3>Artifact</h3>\n");
    out.push_str(&artifact_block(source));
    if let Some(provenance) = source.provenance {
        out.push_str("<h3>Provenance</h3>\n<table class=\"kv\">\n");
        out.push_str(&row(
            "Community snapshot",
            &provenance.community_snapshot_version.to_string(),
        ));
        out.push_str(&row("Created", &provenance.created));
        out.push_str(&row("Software", &provenance.software));
        out.push_str(&row("Root anchor", &provenance.root.anchor_version));
        out.push_str(&rows("Root admissions", &provenance.root.admissions));
        let prep: Vec<String> = provenance
            .prep_versions
            .iter()
            .map(|version| version.to_string())
            .collect();
        out.push_str(&rows("Prep versions", &prep));
        out.push_str("</table>\n");
        out.push_str("<h3>Bodies</h3>\n");
        out.push_str(&body_table(&provenance.bodies));
    }
    out.push_str("</section>\n");
    out
}

fn tone_section(curve: &ToneCurve) -> String {
    format!(
        "<section>\n<h2>Tone</h2>\n{}\n</section>\n",
        svg::tone_curve_svg(curve)
    )
}

fn stats_table(dist: &DeltaEDistribution) -> String {
    let mut out = String::from("<table class=\"kv\">\n");
    out.push_str(&row("Metric", dist.metric.label()));
    out.push_str(&row("Supported probes", &dist.count.to_string()));
    out.push_str(&row("Excluded probes", &dist.excluded.to_string()));
    out.push_str(&row("Median", &number(dist.median, 2)));
    out.push_str(&row("p95", &number(dist.p95, 2)));
    out.push_str(&row("Max", &number(dist.max, 2)));
    out.push_str(&row("Mean", &number(dist.mean, 2)));
    out.push_str(&row("Weighted mean", &number(dist.weighted_mean, 2)));
    out.push_str(&row("Weighted median", &number(dist.weighted_median, 2)));
    out.push_str("</table>\n");
    out
}

fn worst_table(dist: &DeltaEDistribution) -> String {
    if dist.worst.is_empty() {
        return "<p class=\"muted\">No supported probes.</p>\n".to_string();
    }
    let mut out = String::from(
        "<table>\n<thead><tr><th>Probe</th><th>ΔE</th><th>σ</th></tr></thead>\n<tbody>\n",
    );
    for probe in &dist.worst {
        out.push_str(&format!(
            "<tr><td><code>{}</code></td><td>{}</td><td>{}</td></tr>\n",
            escape(probe.id.as_str()),
            escape(&number(probe.delta_e, 2)),
            escape(&number(probe.sigma, 2)),
        ));
    }
    out.push_str("</tbody>\n</table>\n");
    out
}

fn distribution_section(dist: &DeltaEDistribution) -> String {
    format!(
        "<section>\n<h2>{}</h2>\n{}\n<div class=\"columns\">\n<div>\n<h3>Statistics</h3>\n{}</div>\n<div>\n<h3>Worst probes</h3>\n{}</div>\n</div>\n</section>\n",
        escape(dist.metric.label()),
        svg::delta_e_histogram_svg(dist),
        stats_table(dist),
        worst_table(dist),
    )
}

fn region_section(map: &RegionMap) -> String {
    format!(
        "<section>\n<h2>Region map</h2>\n{}\n</section>\n",
        svg::region_map_svg(map)
    )
}

fn attribution_section(comparison: &Comparison) -> String {
    if comparison.attribution.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "<section>\n<h2>Layer attribution</h2>\n<p>Mean ΔE removed by swapping each layer of the second function into the first.</p>\n<table>\n<thead><tr><th>Layer</th><th>Mean ΔE reduction</th></tr></thead>\n<tbody>\n",
    );
    for contribution in &comparison.attribution {
        out.push_str(&format!(
            "<tr><td>{}</td><td>{}</td></tr>\n",
            escape(contribution.layer.name()),
            escape(&number(contribution.contribution, 2)),
        ));
    }
    out.push_str("</tbody>\n</table>\n</section>\n");
    out
}

fn grl_section(grl: Option<&crate::compare::GrlPlot>) -> String {
    match grl {
        Some(plot) => format!(
            "<section>\n<h2>Chroma, hue and lightness</h2>\n{}\n</section>\n",
            svg::grl_svg(plot)
        ),
        None => "<section>\n<h2>Chroma, hue and lightness</h2>\n\
                 <p class=\"muted\">Layer diagnostics need both functions to expose a Look.</p>\n\
                 </section>\n"
            .to_string(),
    }
}

fn has_uncertainty(comparison: &Comparison) -> bool {
    comparison
        .delta_e
        .worst
        .iter()
        .any(|probe| probe.sigma > 0.0)
}

pub fn look_page(source: &ReportSource<'_>, diagnostics: &LookDiagnostics) -> String {
    let title = if source.label.is_empty() {
        "Look report".to_string()
    } else {
        format!("Look report: {}", source.label)
    };
    let mut body = String::new();
    body.push_str(&format!(
        "<header><h1>{}</h1><p class=\"muted\">Look diagnostics, docs/02 §7.</p></header>\n",
        escape(&title)
    ));
    body.push_str(&tone_section(&diagnostics.tone));
    body.push_str(&format!(
        "<section>\n<h2>Neutral tint</h2>\n{}\n</section>\n",
        svg::tint_curve_svg(&diagnostics.tint)
    ));
    body.push_str(&grl_section(diagnostics.grl.as_ref()));
    body.push_str(&source_section(source));
    page(&title, &body)
}

fn tone_comparison_section(plot: &ToneComparison) -> String {
    format!(
        "<section>\n<h2>Tone</h2>\n{}\n</section>\n",
        svg::tone_comparison_svg(plot)
    )
}

pub fn comparison_page(
    comparison: &Comparison,
    a: &ReportSource<'_>,
    b: &ReportSource<'_>,
) -> String {
    let title = format!("Compare {} with {}", comparison.a, comparison.b);
    let mut body = String::new();
    body.push_str(&format!("<header><h1>{}</h1></header>\n", escape(&title)));
    body.push_str(&format!(
        "<section class=\"summary\"><p>{}</p></section>\n",
        escape(&comparison.summary)
    ));
    body.push_str(&tone_comparison_section(&comparison.tone));
    body.push_str(&format!(
        "<section>\n<h2>Neutral tint</h2>\n{}\n</section>\n",
        svg::tint_comparison_svg(&comparison.tint)
    ));
    body.push_str(&grl_section(comparison.grl.as_ref()));
    body.push_str(&distribution_section(&comparison.delta_e));
    body.push_str(&distribution_section(&comparison.ciede2000));
    body.push_str(&region_section(&comparison.region_map));
    body.push_str(&attribution_section(comparison));
    body.push_str("<section>\n<h2>Sources</h2>\n");
    body.push_str(&source_section(a));
    body.push_str(&source_section(b));
    body.push_str("</section>\n");
    if !has_uncertainty(comparison) {
        body.push_str(
            "<p class=\"muted\">No predictive covariance was available; uncertainty bands and σ columns are zero.</p>\n",
        );
    }
    page(&title, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_hash_validation() {
        let hash = ArtifactHash::from_sha256_hex(&"ab".repeat(32));
        assert!(hash.is_some());
        assert!(ArtifactHash::from_sha256_hex("nope").is_none());
        assert!(ArtifactHash::from_sha256_hex(&"AB".repeat(32)).is_none());
    }
}
