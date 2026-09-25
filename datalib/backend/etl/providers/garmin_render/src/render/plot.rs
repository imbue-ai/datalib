//! The weight series as a Plotly page, drawn through the time-series
//! renders' page (`datalib_etl_timeseries_render::plot`).

use anyhow::Result;
use datalib_etl_timeseries_render::plot::{figure_config, figure_page};
use serde_json::json;

use super::parse::WeighIn;

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
        "config": figure_config("weight"),
    });
    figure_page(title, &spec)
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
        assert!(fat.contains(datalib_etl_timeseries_render::plot::PLOTLY_INTEGRITY));
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
