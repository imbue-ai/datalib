//! Build one self-contained Plotly page per physical quantity.
//!
//! ## Shape of the emitted file
//!
//! A single `.html` with no build step and no bundler: a `<div>` for the
//! plot, the figure spec inlined as a `<script type="application/json">`
//! block, and a five-line bootstrap that hands the parsed spec to
//! `Plotly.newPlot`. Opening the file straight off disk works; so does
//! the `<iframe>` in `index.md`.
//!
//! Putting the data in a JSON `<script>` block rather than interpolating
//! it into executable JavaScript means the only escape that matters is
//! `<` (so a device named `</script>` can't break out); [`escape_json_for_html`]
//! handles it, and the browser's JSON parser does the rest. There is no
//! path from stored data into evaluated code.
//!
//! ## Where Plotly comes from
//!
//! [`PLOTLY_SRC`] — a pinned version on Plotly's CDN, guarded by a
//! Subresource Integrity hash so a compromised or swapped CDN artifact
//! fails closed rather than executing. The consequence is that plots
//! need network access **when viewed**; [`OFFLINE_NOTICE`] is what the
//! reader gets when the fetch fails, instead of a blank frame.
//!
//! To make the plots work offline instead, write the library into the
//! rendered tree once and point [`PLOTLY_SRC`] at it relative to the
//! plot: everything else here is already relative-path clean.

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

use super::units::{Axis, Quantity};

/// Pinned Plotly build. Version-pinned rather than floating (`latest`)
/// so a rendered page keeps behaving the way it did the day it was
/// written, and so the integrity hash below stays valid.
pub const PLOTLY_SRC: &str = "https://cdn.plot.ly/plotly-3.1.0.min.js";

/// SHA-384 Subresource Integrity hash of [`PLOTLY_SRC`]. Must be
/// recomputed whenever the pin moves:
///
/// ```sh
/// curl -s https://cdn.plot.ly/plotly-<ver>.min.js \
///   | openssl dgst -sha384 -binary | openssl base64 -A
/// ```
pub const PLOTLY_INTEGRITY: &str =
    "sha384-DAxS2fhSGacPW3IdpTjDpu+KotwjM8aHsfrkZRnfYyJIhAHoDav7jAJ+NmYcp6PL";

/// Shown in place of the plot when the CDN script didn't load.
pub const OFFLINE_NOTICE: &str = "This plot draws with Plotly, loaded from cdn.plot.ly, \
     which could not be reached. Reconnect and reload to see the chart; \
     the data itself is inlined in this file and is not lost.";

/// One device-and-metric series, already converted to SI.
///
/// # Why the line is not broken across gaps
///
/// A connected line spanning a stretch with no readings looks like it is
/// asserting data nobody measured, so breaking it across outages is a
/// tempting addition. It was tried and removed, because these series
/// have no outages to detect — only long tails.
///
/// Measured against the live store (2026-08-21): every series' interval
/// distribution runs from a 1.9–60 minute median out to a 2–5 hour
/// maximum, with nothing bimodal in between. The water meter is the
/// clearest case — median 1.9 min, p95 45 min — because it reports on
/// activity rather than on a clock. A "gap longer than 10x the median"
/// rule flagged 11% of its perfectly normal intervals as outages and
/// shattered its line into ~1400 pieces. Any threshold that leaves that
/// series intact is high enough to fire on nothing else.
///
/// What makes this safe is the markers: `lines+markers` draws a dot at
/// every real sample, so a long bare segment with no dots on it reads as
/// "nothing was recorded here" on sight. Keep the markers, and the line
/// cannot lie about density.
pub struct Trace {
    /// Legend label.
    pub name: String,
    pub axis: Axis,
    /// Unix milliseconds. Passed to Plotly as numbers against a `date`
    /// axis — which it reads as ms since the Unix epoch, UTC — rather
    /// than as ISO strings. At ~60k points on the temperature plot the
    /// string form would roughly double the file for no added
    /// information.
    pub x_ms: Vec<i64>,
    /// SI values, parallel to `x_ms`.
    pub y: Vec<f64>,
    /// Unit suffix for the hover readout, e.g. `°C`.
    pub unit: String,
}

/// Render a standalone page plotting `traces` for `quantity`.
///
/// `subtitle` lands under the plot title — the caller uses it for the
/// point count and the covered time range.
pub fn standalone_html(quantity: &Quantity, subtitle: &str, traces: &[Trace]) -> Result<String> {
    let data: Vec<Value> = traces.iter().map(trace_json).collect();
    let spec = json!({
        "data": data,
        "layout": layout_json(quantity, subtitle),
        "config": {
            "responsive": true,
            "displaylogo": false,
            "scrollZoom": true,
            "toImageButtonOptions": {"filename": quantity.key, "format": "png", "scale": 2},
        },
    });
    let spec_json = escape_json_for_html(
        &serde_json::to_string(&spec).context("serialize plotly figure spec")?,
    );

    let title = html_escape(quantity.title);
    let notice = html_escape(OFFLINE_NOTICE);
    Ok(format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<script src="{src}" integrity="{integrity}" crossorigin="anonymous" referrerpolicy="no-referrer"></script>
<style>
  html, body {{ margin: 0; padding: 0; height: 100%; }}
  body {{
    font: 14px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif;
    background: #fff; color: #111;
  }}
  #plot {{ width: 100%; height: 100%; }}
  #offline {{ display: none; margin: 2rem; padding: 1rem 1.25rem;
              border: 1px solid #e0c000; border-radius: 6px; background: #fffbe6; }}
  @media (prefers-color-scheme: dark) {{
    body {{ background: #16161a; color: #eee; }}
    #offline {{ background: #2c2612; border-color: #7a6a10; }}
  }}
</style>
</head>
<body>
<div id="plot"></div>
<p id="offline">{notice}</p>
<script id="figure" type="application/json">{spec_json}</script>
<script>
(function () {{
  var spec = JSON.parse(document.getElementById("figure").textContent);
  if (typeof Plotly === "undefined") {{
    document.getElementById("plot").style.display = "none";
    document.getElementById("offline").style.display = "block";
    return;
  }}
  Plotly.newPlot("plot", spec.data, spec.layout, spec.config);
}})();
</script>
</body>
</html>
"#,
        src = html_escape(PLOTLY_SRC),
        integrity = html_escape(PLOTLY_INTEGRITY),
    ))
}

fn trace_json(t: &Trace) -> Value {
    let mut m = Map::new();
    // `scattergl` (WebGL) rather than `scatter` (SVG): the temperature
    // and humidity plots carry tens of thousands of points each, where
    // an SVG trace turns panning and zooming into a slideshow.
    m.insert("type".into(), json!("scattergl"));
    // Markers plus the connecting line: the markers say where the
    // samples actually are (which matters when the sampling interval
    // varies), the line carries the shape between them.
    m.insert("mode".into(), json!("lines+markers"));
    m.insert("name".into(), json!(t.name));
    m.insert("x".into(), json!(t.x_ms));
    m.insert("y".into(), json!(t.y));
    m.insert("marker".into(), json!({"size": 3}));
    // Thin: at tens of thousands of points a default-width line fills
    // in solid and hides the markers under it.
    m.insert("line".into(), json!({"width": 1}));
    m.insert(
        "hovertemplate".into(),
        // `<extra>` holds the series name in the hover box's side panel.
        json!(format!(
            "%{{x|%Y-%m-%d %H:%M}} · %{{y:.3f}} {}<extra>{}</extra>",
            t.unit, t.name
        )),
    );
    if t.axis == Axis::Right {
        m.insert("yaxis".into(), json!("y2"));
    }
    Value::Object(m)
}

fn layout_json(quantity: &Quantity, subtitle: &str) -> Value {
    let mut layout = json!({
        "title": {"text": format!("{}<br><sub>{}</sub>", quantity.title, subtitle)},
        "xaxis": {"type": "date", "title": {"text": "Time (UTC)"}, "automargin": true},
        "yaxis": {"title": {"text": quantity.left_unit}, "automargin": true},
        "hovermode": "closest",
        "legend": {"orientation": "h", "y": -0.18, "x": 0},
        "margin": {"l": 60, "r": 60, "t": 70, "b": 60},
        "template": {"layout": {"colorway": [
            "#1f77b4", "#d62728", "#2ca02c", "#ff7f0e",
            "#9467bd", "#8c564b", "#17becf", "#e377c2"
        ]}},
    });
    if let Some(right) = quantity.right_unit {
        layout["yaxis2"] = json!({
            "title": {"text": right},
            "overlaying": "y",
            "side": "right",
            "automargin": true,
            // Zero-anchored so the totalizer's climb reads as a fraction
            // of its own range rather than as an arbitrary offset.
            "rangemode": "tozero",
        });
    }
    layout
}

/// Make a JSON document safe to embed in a `<script>` element.
///
/// The HTML parser ends a `<script>` at the first `</script` regardless
/// of JSON quoting, so a stored string containing one would truncate the
/// figure. Escaping every `<` as `\u003c` — still valid JSON, still
/// parses back to `<` — closes that off without needing to reason about
/// where in the document the character appeared.
pub fn escape_json_for_html(json: &str) -> String {
    json.replace('<', "\\u003c")
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::units::{TEMPERATURE, VOLUME};

    fn trace(name: &str, axis: Axis) -> Trace {
        Trace {
            name: name.to_string(),
            axis,
            x_ms: vec![1_700_000_000_000, 1_700_000_060_000],
            y: vec![1.5, 2.5],
            unit: "°C".into(),
        }
    }

    #[test]
    fn single_axis_plot_declares_no_y2() {
        let html =
            standalone_html(&TEMPERATURE, "2 points", &[trace("fridge", Axis::Left)]).unwrap();
        assert!(html.contains("scattergl"), "{html}");
        assert!(
            !html.contains("yaxis2"),
            "temperature has no secondary axis"
        );
        assert!(html.contains(PLOTLY_INTEGRITY));
        assert!(html.contains(PLOTLY_SRC));
    }

    #[test]
    fn secondary_axis_appears_only_for_quantities_that_declare_one() {
        let html = standalone_html(
            &VOLUME,
            "2 points",
            &[
                trace("valve (consumption)", Axis::Left),
                trace("valve (meter total)", Axis::Right),
            ],
        )
        .unwrap();
        assert!(html.contains("yaxis2"), "{html}");
        assert!(html.contains("overlaying"), "{html}");
        // The right-hand trace must actually be bound to y2 — without
        // this the axis exists but nothing draws on it.
        assert!(
            html.contains(r#"\"yaxis\":\"y2\""#) || html.contains(r#""yaxis":"y2""#),
            "{html}"
        );
    }

    #[test]
    fn a_device_named_like_a_script_tag_cannot_break_out() {
        // The escape that matters: `</script>` inside stored data would
        // otherwise terminate the JSON block early and leave the rest of
        // the figure sitting in the document as markup.
        let html = standalone_html(
            &TEMPERATURE,
            "sub",
            &[trace("</script><img src=x onerror=alert(1)>", Axis::Left)],
        )
        .unwrap();
        let body = html.split_once(r#"type="application/json">"#).unwrap().1;
        let figure = body.split_once("</script>").unwrap().0;
        assert!(!figure.contains("</script>"), "figure block was truncated");
        assert!(figure.contains("\\u003c/script"), "{figure}");
        // And it round-trips: the escaped form is still valid JSON that
        // parses back to the original name.
        let parsed: serde_json::Value = serde_json::from_str(figure).unwrap();
        assert_eq!(
            parsed["data"][0]["name"].as_str().unwrap(),
            "</script><img src=x onerror=alert(1)>"
        );
    }

    #[test]
    fn timestamps_are_numbers_against_a_date_axis() {
        let html = standalone_html(&TEMPERATURE, "sub", &[trace("d", Axis::Left)]).unwrap();
        assert!(
            html.contains("1700000000000"),
            "epoch ms should be inlined as a number"
        );
        assert!(html.contains(r#""type":"date""#) || html.contains(r#"\"type\":\"date\""#));
    }

    #[test]
    fn samples_are_joined_by_a_thin_line() {
        let html = standalone_html(&TEMPERATURE, "sub", &[trace("d", Axis::Left)]).unwrap();
        assert!(html.contains(r#""mode":"lines+markers""#), "{html}");
        assert!(html.contains(r#""line":{"width":1}"#), "{html}");
    }

    #[test]
    fn every_sample_is_plotted_and_the_line_is_never_broken() {
        // The counterpart to `Trace`'s docs on gaps: no nulls in the y
        // array, and `connectgaps` left unset. If someone reintroduces
        // gap-breaking, this fails and sends them to the measurement
        // that says why it was removed.
        let mut t = trace("d", Axis::Left);
        t.x_ms = vec![1, 2, 3];
        t.y = vec![1.0, 2.0, 3.0];
        let html = standalone_html(&TEMPERATURE, "sub", &[t]).unwrap();
        let body = html.split_once(r#"type="application/json">"#).unwrap().1;
        let figure = body.split_once("</script>").unwrap().0;
        let parsed: serde_json::Value = serde_json::from_str(figure).unwrap();
        let ys = parsed["data"][0]["y"].as_array().unwrap();
        assert_eq!(ys.len(), 3);
        assert!(ys.iter().all(|v| v.is_number()), "{ys:?}");
        assert!(!html.contains("connectgaps"), "{html}");
    }

    #[test]
    fn offline_notice_is_present_so_a_failed_cdn_fetch_is_not_a_blank_page() {
        let html = standalone_html(&TEMPERATURE, "sub", &[trace("d", Axis::Left)]).unwrap();
        assert!(html.contains("cdn.plot.ly"));
        assert!(html.contains("id=\"offline\""));
    }
}
