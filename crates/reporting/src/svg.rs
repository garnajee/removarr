//! Chart geometry for the released-space charts. This module only computes coordinates (SVG
//! path data, ticks, marker and dash style per series); the markup lives in a web template.
//! Series differ by dash pattern and marker shape as well as colour, and every point carries
//! its date and value so a table alternative can be rendered from the same data.

use serde::Serialize;

use crate::model::{SeriesKind, SeriesVm};

const MARGIN_LEFT: f64 = 64.0;
const MARGIN_RIGHT: f64 = 16.0;
const MARGIN_TOP: f64 = 16.0;
const MARGIN_BOTTOM: f64 = 32.0;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PointGeometry {
    pub x: f64,
    pub y: f64,
    pub at_iso: String,
    pub value: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LineGeometry {
    pub kind: SeriesKind,
    pub label_key: &'static str,
    /// SVG path data (`M x y L x y ...`), empty when the series has no point.
    pub path_d: String,
    /// `stroke-dasharray` value, distinct per series.
    pub dash: &'static str,
    /// `circle`, `square`, `diamond`, `triangle` or `cross`.
    pub marker: &'static str,
    pub points: Vec<PointGeometry>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Tick {
    pub position: f64,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChartGeometry {
    pub width: u32,
    pub height: u32,
    pub plot_left: f64,
    pub plot_right: f64,
    pub plot_top: f64,
    pub plot_bottom: f64,
    pub y_min: i64,
    pub y_max: i64,
    pub x_min: i64,
    pub x_max: i64,
    pub lines: Vec<LineGeometry>,
    pub y_ticks: Vec<Tick>,
    pub x_ticks: Vec<Tick>,
    /// True when no series has any point: render the "no data" message, not an empty axis.
    pub empty: bool,
}

fn style(kind: SeriesKind) -> (&'static str, &'static str) {
    match kind {
        SeriesKind::Potential => ("2 4", "diamond"),
        SeriesKind::Expected => ("6 3", "square"),
        SeriesKind::Attributable => ("", "circle"),
        SeriesKind::AddedCost => ("1 3", "cross"),
        SeriesKind::Observed => ("8 3 2 3", "triangle"),
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Computes the geometry of the cumulative values of `series` in a `width` x `height` box.
#[must_use]
pub fn chart_geometry(series: &[SeriesVm], width: u32, height: u32) -> ChartGeometry {
    chart_geometry_for(series, width, height, false)
}

/// Like [`chart_geometry`]; with `per_day` the plotted value is each day's own bytes instead of
/// the running total.
#[must_use]
pub fn chart_geometry_for(
    series: &[SeriesVm],
    width: u32,
    height: u32,
    per_day: bool,
) -> ChartGeometry {
    let val = |p: &crate::model::SeriesPointVm| if per_day { p.bytes } else { p.cumulative };
    let (w, h) = (f64::from(width.max(200)), f64::from(height.max(120)));
    let (left, right, top, bottom) = (MARGIN_LEFT, w - MARGIN_RIGHT, MARGIN_TOP, h - MARGIN_BOTTOM);
    let all: Vec<&crate::model::SeriesPointVm> =
        series.iter().flat_map(|s| s.points.iter()).collect();
    let empty = all.is_empty();
    let x_min = all.iter().map(|p| p.at).min().unwrap_or(0);
    let x_max = all.iter().map(|p| p.at).max().unwrap_or(0);
    let y_min = all.iter().map(|p| val(p)).min().unwrap_or(0).min(0);
    let y_max = all
        .iter()
        .map(|p| val(p))
        .max()
        .unwrap_or(0)
        .max(0)
        .max(y_min + 1);
    let xs = |at: i64| -> f64 {
        if x_max == x_min {
            (left + right) / 2.0
        } else {
            left + (right - left) * ((at - x_min) as f64 / (x_max - x_min) as f64)
        }
    };
    let ys =
        |v: i64| -> f64 { bottom - (bottom - top) * ((v - y_min) as f64 / (y_max - y_min) as f64) };
    let lines = series
        .iter()
        .map(|s| {
            let points: Vec<PointGeometry> = s
                .points
                .iter()
                .map(|p| PointGeometry {
                    x: round1(xs(p.at)),
                    y: round1(ys(val(p))),
                    at_iso: p.at_iso.clone(),
                    value: val(p),
                })
                .collect();
            let path_d = points
                .iter()
                .enumerate()
                .map(|(i, p)| format!("{} {} {}", if i == 0 { "M" } else { "L" }, p.x, p.y))
                .collect::<Vec<_>>()
                .join(" ");
            let (dash, marker) = style(s.kind);
            LineGeometry {
                kind: s.kind,
                label_key: s.label_key,
                path_d,
                dash,
                marker,
                points,
            }
        })
        .collect();
    let y_ticks = (0..=4)
        .map(|i| {
            let v = y_min + (y_max - y_min) * i / 4;
            Tick {
                position: round1(ys(v)),
                label: v.to_string(),
            }
        })
        .collect();
    let x_ticks = if empty {
        vec![]
    } else {
        let mut ats: Vec<i64> = all.iter().map(|p| p.at).collect();
        ats.sort_unstable();
        ats.dedup();
        let step = ats.len().div_ceil(5).max(1);
        ats.iter()
            .step_by(step)
            .map(|at| Tick {
                position: round1(xs(*at)),
                label: crate::timefmt::iso(*at).chars().take(10).collect(),
            })
            .collect()
    };
    ChartGeometry {
        width: width.max(200),
        height: height.max(120),
        plot_left: left,
        plot_right: right,
        plot_top: top,
        plot_bottom: bottom,
        y_min,
        y_max,
        x_min,
        x_max,
        lines,
        y_ticks,
        x_ticks,
        empty,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::model::SeriesPointVm;

    fn series(kind: SeriesKind, pts: &[(i64, i64)]) -> SeriesVm {
        SeriesVm {
            kind,
            label_key: kind.label_key(),
            label: kind.label_en(),
            points: pts
                .iter()
                .map(|(at, c)| SeriesPointVm {
                    at: *at,
                    at_iso: crate::timefmt::iso(*at),
                    bytes: *c,
                    cumulative: *c,
                })
                .collect(),
            total: pts.last().map_or(0, |p| p.1),
            guaranteed: true,
        }
    }

    #[test]
    fn empty_input_is_flagged() {
        let g = chart_geometry(&[series(SeriesKind::Attributable, &[])], 600, 300);
        assert!(g.empty && g.x_ticks.is_empty());
        assert_eq!(g.lines[0].path_d, "");
    }

    #[test]
    fn points_stay_inside_the_plot_and_series_differ_by_style() {
        let g = chart_geometry(
            &[
                series(SeriesKind::Attributable, &[(0, 0), (86_400_000, 100)]),
                series(SeriesKind::Observed, &[(0, 0), (86_400_000, -50)]),
            ],
            600,
            300,
        );
        for l in &g.lines {
            for p in &l.points {
                assert!(
                    p.x >= g.plot_left
                        && p.x <= g.plot_right
                        && p.y >= g.plot_top
                        && p.y <= g.plot_bottom,
                    "{p:?}"
                );
            }
        }
        assert_ne!(g.lines[0].dash, g.lines[1].dash);
        assert_ne!(g.lines[0].marker, g.lines[1].marker);
        assert!(g.lines[0].path_d.starts_with("M "));
        assert_eq!((g.y_min, g.y_max), (-50, 100));
    }

    #[test]
    fn a_single_point_is_centered() {
        let g = chart_geometry(&[series(SeriesKind::Potential, &[(5, 10)])], 600, 300);
        assert_eq!(g.lines[0].points[0].x, (g.plot_left + g.plot_right) / 2.0);
    }
}
