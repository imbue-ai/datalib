//! One self-contained Plotly page for the weight series.

use anyhow::{Context, Result};
use serde_json::json;

use super::parse::WeighIn;

/// Pinned Plotly build and its SRI hash — the same pair the yolink
/// renderer pins, so the two pages behave alike and one bump moves both.
pub const PLOTLY_SRC: &str = "https://cdn.plot.ly/plotly-3.1.0.min.js";
pub const PLOTLY_INTEGRITY: &str =
    "sha384-DAxS2fhSGacPW3IdpTjDpu+KotwjM8aHsfrkZRnfYyJIhAHoDav7jAJ+NmYcp6PL";

pub const OFFLINE_NOTICE: &str = "This plot draws with Plotly, loaded from cdn.plot.ly, \
     which could not be reached. Reconnect and reload to see the chart; \
     the data itself is inlined in this file and is not lost.";

pub fn weight_html(title: &str, subtitle: &str, weigh_ins: &[WeighIn]) -> Result<String> {
    let x: Vec<i64> = weigh_ins.iter().map(|w| w.timestamp_gmt_ms).collect();
    let mut data = vec![json!({
        "type": "scatter",
        "mode": "lines+markers",
        "name": "Weight",
        "x": x,
        "y": weigh_ins.iter().map(|w| w.weight_kg).collect::<Vec<_>>(),
        "marker": {"size": 5},
        "line": {"width": 1.5},
        "hovertemplate": "%{x|%Y-%m-%d %H:%M} · %{y:.1f} kg<extra>Weight</extra>",
    })];
    let fat: Vec<(i64, f64)> = weigh_ins
        .iter()
        .filter_map(|w| w.body_fat_pct.map(|f| (w.timestamp_gmt_ms, f)))
        .collect();
    let has_fat = !fat.is_empty();
    if has_fat {
        data.push(json!({
            "type": "scatter",
            "mode": "lines+markers",
            "name": "Body fat",
            "x": fat.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
            "y": fat.iter().map(|(_, f)| *f).collect::<Vec<_>>(),
            "yaxis": "y2",
            "marker": {"size": 4},
            "line": {"width": 1, "dash": "dot"},
            "hovertemplate": "%{x|%Y-%m-%d %H:%M} · %{y:.1f} %<extra>Body fat</extra>",
        }));
    }
    let mut layout = json!({
        "title": {"text": format!("{title}<br><sub>{subtitle}</sub>")},
        "xaxis": {"type": "date", "title": {"text": "Date"}, "automargin": true},
        "yaxis": {"title": {"text": "kg"}, "automargin": true},
        "hovermode": "closest",
        "legend": {"orientation": "h", "y": -0.18, "x": 0},
        "margin": {"l": 60, "r": 60, "t": 70, "b": 60},
    });
    if has_fat {
        layout["yaxis2"] = json!({
            "title": {"text": "% body fat"},
            "overlaying": "y",
            "side": "right",
            "automargin": true,
        });
    }
    let spec = json!({
        "data": data,
        "layout": layout,
        "config": {
            "responsive": true,
            "displaylogo": false,
            "scrollZoom": true,
            "toImageButtonOptions": {"filename": "weight", "format": "png", "scale": 2},
        },
    });
    let spec_json = escape_json_for_html(
        &serde_json::to_string(&spec).context("serialize plotly figure spec")?,
    );
    Ok(page(&html_escape(title), &spec_json))
}

fn page(title: &str, spec_json: &str) -> String {
    let notice = html_escape(OFFLINE_NOTICE);
    format!(
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
    )
}

/// Make a JSON document safe to embed in a `<script>` element.
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

    fn w(ts: i64, kg: f64, fat: Option<f64>) -> WeighIn {
        WeighIn {
            id: ts.to_string(),
            calendar_date: "2369-04-14".into(),
            timestamp_gmt_ms: ts,
            weight_kg: kg,
            bmi: None,
            body_fat_pct: fat,
            source_type: None,
        }
    }

    #[test]
    fn body_fat_draws_on_a_second_axis_only_when_present() {
        let plain = weight_html("Weight", "sub", &[w(1, 78.0, None), w(2, 77.5, None)]).unwrap();
        assert!(!plain.contains("yaxis2"), "{plain}");
        let fat = weight_html("Weight", "sub", &[w(1, 78.0, Some(15.0))]).unwrap();
        assert!(fat.contains("yaxis2"), "{fat}");
        assert!(fat.contains(r#""yaxis":"y2""#), "{fat}");
        assert!(fat.contains(PLOTLY_INTEGRITY));
    }

    #[test]
    fn a_title_cannot_break_out_of_the_figure_block() {
        let html = weight_html("</script><b>", "sub", &[w(1, 78.0, None)]).unwrap();
        let body = html.split_once(r#"type="application/json">"#).unwrap().1;
        let figure = body.split_once("</script>").unwrap().0;
        assert!(!figure.contains("</script>"), "figure block was truncated");
        let parsed: serde_json::Value = serde_json::from_str(figure).unwrap();
        assert!(parsed["layout"]["title"]["text"]
            .as_str()
            .unwrap()
            .starts_with("</script>"));
    }
}
