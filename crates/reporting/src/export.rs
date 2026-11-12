//! Exports of a [`ReportBundle`]: human-readable Markdown or HTML, machine-readable JSON and
//! CSV. Every text cell is sanitized (no secret, no URL) and then escaped for its format.
//!
//! CSV cells that could start a spreadsheet formula (`=`, `+`, `-`, `@`, tab, CR) are prefixed
//! with an apostrophe. HTML output has no script, no external reference and a restrictive
//! meta CSP; every dynamic value is escaped.

use removarr_domain::error::{ErrorClass, PortError};

use crate::model::*;
use crate::redact::{sanitize, sanitize_json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Json,
    /// Issues table (the legacy candidates CSV).
    CsvIssues,
    CsvHistory,
    CsvSeries,
    Markdown,
    Html,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportFile {
    pub file_name: String,
    pub content_type: &'static str,
    pub bytes: Vec<u8>,
}

/// File name safe for a download header: `removarr-<kind>-<yyyymmddThhmmssZ>.<ext>`.
fn stamp(b: &ReportBundle) -> String {
    b.generated_at_iso
        .chars()
        .filter(char::is_ascii_digit)
        .take(14)
        .collect::<String>()
}

pub fn render(b: &ReportBundle, f: ExportFormat) -> Result<ExportFile, PortError> {
    let (name, ct, text) = match f {
        ExportFormat::Json => ("report", "application/json; charset=utf-8", to_json(b)?),
        ExportFormat::CsvIssues => ("issues", "text/csv; charset=utf-8", issues_csv(b)),
        ExportFormat::CsvHistory => ("history", "text/csv; charset=utf-8", history_csv(b)),
        ExportFormat::CsvSeries => ("series", "text/csv; charset=utf-8", series_csv(b)),
        ExportFormat::Markdown => ("report", "text/markdown; charset=utf-8", to_markdown(b)),
        ExportFormat::Html => ("report", "text/html; charset=utf-8", to_html(b)),
    };
    let ext = match f {
        ExportFormat::Json => "json",
        ExportFormat::Markdown => "md",
        ExportFormat::Html => "html",
        _ => "csv",
    };
    Ok(ExportFile {
        file_name: format!("removarr-{name}-{}Z.{ext}", stamp(b)),
        content_type: ct,
        bytes: text.into_bytes(),
    })
}

/// Pretty JSON, with a last sanitizing pass over every string.
pub fn to_json(b: &ReportBundle) -> Result<String, PortError> {
    let v = serde_json::to_value(b)
        .map_err(|_| PortError::new(ErrorClass::Internal, "report is not serializable"))?;
    serde_json::to_string_pretty(&sanitize_json(&v))
        .map_err(|_| PortError::new(ErrorClass::Internal, "report is not serializable"))
}

/// One CSV text cell: sanitized, single-line, formula-neutralized, RFC 4180 quoted.
#[must_use]
pub fn csv_cell(text: &str) -> String {
    let flat: String = sanitize(text)
        .chars()
        .map(|c| if c.is_control() && c != '\t' { ' ' } else { c })
        .collect();
    let guarded = if flat.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{flat}")
    } else {
        flat
    };
    if guarded.contains([',', '"', ';', '\t']) || guarded.starts_with(' ') || guarded.ends_with(' ')
    {
        format!("\"{}\"", guarded.replace('"', "\"\""))
    } else {
        guarded
    }
}

fn csv(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut out = headers.join(",");
    out.push_str("\r\n");
    for r in rows {
        out.push_str(&r.iter().map(|c| csv_cell(c)).collect::<Vec<_>>().join(","));
        out.push_str("\r\n");
    }
    out
}

fn n(v: impl ToString) -> String {
    v.to_string()
}

#[must_use]
pub fn issues_csv(b: &ReportBundle) -> String {
    let rows: Vec<Vec<String>> = b
        .issues
        .rows
        .iter()
        .map(|i| {
            vec![
                i.id.clone(),
                enum_key(&i.category),
                enum_key(&i.severity),
                enum_key(&i.outcome),
                i.unit_kind.to_string(),
                i.name.clone(),
                i.paths.join(" | "),
                n(i.logical_bytes),
                n(i.potential_bytes),
                n(i.torrent_count),
                i.blockers.join(" | "),
                i.completeness.state.to_string(),
                i.completeness.missing.join(" | "),
                i.inodes.join(" | "),
                i.first_seen_at_iso.clone(),
            ]
        })
        .collect();
    csv(
        &[
            "id",
            "category",
            "severity",
            "outcome",
            "unit",
            "name",
            "paths",
            "logical_bytes",
            "potential_bytes_estimate_not_guaranteed",
            "torrent_count",
            "blockers",
            "completeness",
            "missing",
            "inodes",
            "first_seen_at",
        ],
        &rows,
    )
}

#[must_use]
pub fn history_csv(b: &ReportBundle) -> String {
    let rows: Vec<Vec<String>> = b
        .history
        .rows
        .iter()
        .map(|h| {
            vec![
                h.at_iso.clone(),
                h.started_at_iso.clone().unwrap_or_default(),
                h.plan_id.clone(),
                n(h.action_index),
                h.action_kind.clone(),
                h.target.clone(),
                h.reason.origin.clone(),
                h.reason.approved_by.clone().unwrap_or_default(),
                h.outcome.key.clone(),
                h.outcome.detail.clone().unwrap_or_default(),
                n(h.attributable_bytes),
                n(h.added_bytes),
                h.gain_ids.join(" | "),
            ]
        })
        .collect();
    csv(
        &[
            "at",
            "started_at",
            "plan_id",
            "action_index",
            "action_kind",
            "target",
            "origin",
            "approved_by",
            "outcome",
            "outcome_detail",
            "attributable_bytes",
            "added_bytes",
            "gain_ids",
        ],
        &rows,
    )
}

#[must_use]
pub fn series_csv(b: &ReportBundle) -> String {
    let rows: Vec<Vec<String>> = b
        .stats
        .series
        .iter()
        .flat_map(|s| {
            s.points.iter().map(move |p| {
                vec![
                    enum_key(&s.kind),
                    p.at_iso.clone(),
                    n(p.bytes),
                    n(p.cumulative),
                    n(s.guaranteed),
                ]
            })
        })
        .collect();
    csv(
        &[
            "series",
            "bucket_start",
            "bytes",
            "cumulative",
            "guaranteed",
        ],
        &rows,
    )
}

fn enum_key<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|x| x.as_str().map(str::to_string))
        .unwrap_or_default()
}

// ---------------------------------------------------------------- document model

struct Table {
    headers: Vec<&'static str>,
    rows: Vec<Vec<String>>,
}

enum Block {
    Heading(String),
    Para(String),
    Bullets(Vec<String>),
    Table(Table),
}

fn reason_text(r: &ReasonVm) -> String {
    let base = match r.key {
        "critical_issues" => "critical issues present",
        "scan_missing" => "no scan has finished",
        "scan_incomplete" => "the last scan is incomplete",
        "scan_stale" => "the last scan is older than twice the scan interval",
        "service_not_ok" => "a configured service is not OK",
        "torrents_left_stopped" => "torrents were left stopped",
        "recovery_pending" => "recovery is pending",
        "auto_paused" => "automatic mode is paused",
        "warning_issues" => "warnings present",
        other => other,
    };
    match r.count {
        Some(c) => format!("{base} ({c})"),
        None => base.to_string(),
    }
}

fn document(b: &ReportBundle) -> (String, Vec<Block>) {
    let d = &b.dashboard;
    let mut v = vec![Block::Para(format!("Generated at {}.", b.generated_at_iso))];
    match &b.session {
        Some(s) => v.push(Block::Para(format!(
            "Scan session {} ({}), started {}, finished {}. Everything below is a record of that moment, not the current state.",
            s.id,
            s.status,
            s.started_at_iso,
            s.finished_at_iso.clone().unwrap_or_else(|| "not finished".into())
        ))),
        None => v.push(Block::Para("No scan session is available.".into())),
    }
    v.push(Block::Heading("Status".into()));
    let mut st = vec![format!("{} ({})", d.urgency.label, d.urgency.icon)];
    st.extend(d.urgency.reasons.iter().map(reason_text));
    v.push(Block::Bullets(st));
    if !b.explanations.is_empty() {
        v.push(Block::Heading("Incomplete data and caveats".into()));
        v.push(Block::Bullets(
            b.explanations.iter().map(|e| e.text.clone()).collect(),
        ));
    }
    v.push(Block::Heading("Space".into()));
    let sp = &d.space;
    v.push(Block::Table(Table {
        headers: vec!["Quantity", "Bytes", "Meaning"],
        rows: vec![
            vec![
                "Logical size".into(),
                n(sp.logical_bytes),
                "Sum of file sizes over every path; a hardlinked file counts once per path.".into(),
            ],
            vec![
                "Unique allocation".into(),
                n(sp.unique_allocated_bytes),
                "Allocated bytes, each inode once.".into(),
            ],
            vec![
                "Shared space".into(),
                n(sp.shared_bytes),
                "Allocated bytes of inodes reachable through two or more paths.".into(),
            ],
            vec![
                "Potential gain".into(),
                n(sp.potential_bytes),
                POTENTIAL_LABEL_EN.into(),
            ],
        ],
    }));
    if sp.potential_overlap_excluded_bytes > 0 {
        v.push(Block::Para(format!(
            "{} potential byte(s) were not added because other issues already count the same inodes.",
            sp.potential_overlap_excluded_bytes
        )));
    }
    if let Some(at) = &sp.observed_at_iso {
        v.push(Block::Para(format!("Inventory observed at {at}.")));
    }
    v.push(Block::Heading("Issues by category".into()));
    v.push(Block::Table(Table {
        headers: vec![
            "Category",
            "Count",
            "Critical",
            "Warning",
            "Info",
            "Logical bytes",
            "Potential bytes (estimate)",
        ],
        rows: d
            .categories
            .iter()
            .map(|c| {
                vec![
                    enum_key(&c.category),
                    n(c.count),
                    n(c.critical),
                    n(c.warning),
                    n(c.info),
                    n(c.logical_bytes),
                    n(c.potential_bytes),
                ]
            })
            .collect(),
    }));
    v.push(Block::Heading("Services".into()));
    v.push(Block::Table(Table {
        headers: vec!["Service", "State", "Version", "Observed at", "Live"],
        rows: d
            .services
            .iter()
            .map(|s| {
                vec![
                    s.service.clone(),
                    s.state_key.to_string(),
                    s.version.clone().unwrap_or_default(),
                    s.observed_at_iso.clone(),
                    if s.current {
                        "yes"
                    } else {
                        "no (recorded by the scan)"
                    }
                    .into(),
                ]
            })
            .collect(),
    }));
    v.push(Block::Heading("Automatic mode".into()));
    v.push(Block::Para(format!(
        "Enabled: {}. Paused: {}.{}",
        if d.auto.enabled { "yes" } else { "no" },
        if d.auto.paused { "yes" } else { "no" },
        d.auto
            .paused_reason
            .as_ref()
            .map(|r| format!(" Reason: {r}."))
            .unwrap_or_default()
    )));
    v.push(Block::Heading("Issues".into()));
    v.push(Block::Table(Table {
        headers: vec![
            "Severity",
            "Category",
            "Name",
            "Logical bytes",
            "Potential bytes (estimate)",
            "Completeness",
            "Blockers",
        ],
        rows: b
            .issues
            .rows
            .iter()
            .take(500)
            .map(|i| {
                vec![
                    enum_key(&i.severity),
                    enum_key(&i.category),
                    i.name.clone(),
                    n(i.logical_bytes),
                    n(i.potential_bytes),
                    i.completeness.state.to_string(),
                    i.blockers.join(", "),
                ]
            })
            .collect(),
    }));
    v.push(Block::Heading("Gains".into()));
    let t = &b.stats.totals;
    v.push(Block::Table(Table {
        headers: vec!["Measure", "Bytes"],
        rows: vec![
            vec![
                "Attributable (unique gain ids)".into(),
                n(t.attributable_bytes),
            ],
            vec!["Added cost".into(), n(t.added_bytes)],
            vec!["Net".into(), n(t.net_bytes)],
        ],
    }));
    v.push(Block::Table(Table {
        headers: vec!["Series", "Total bytes", "Measured"],
        rows: b
            .stats
            .series
            .iter()
            .map(|s| {
                vec![
                    s.label.to_string(),
                    n(s.total),
                    if s.guaranteed {
                        "measured"
                    } else {
                        "estimate or mixed"
                    }
                    .into(),
                ]
            })
            .collect(),
    }));
    v.push(Block::Heading("History".into()));
    v.push(Block::Table(Table {
        headers: vec![
            "At",
            "Plan",
            "Action",
            "Target",
            "Origin",
            "Outcome",
            "Attributable bytes",
            "Gain ids",
        ],
        rows: b
            .history
            .rows
            .iter()
            .take(500)
            .map(|h| {
                vec![
                    h.at_iso.clone(),
                    h.plan_id.clone(),
                    h.action_kind.clone(),
                    h.target.clone(),
                    h.reason.origin.clone(),
                    h.outcome.key.clone(),
                    n(h.attributable_bytes),
                    h.gain_ids.join(" "),
                ]
            })
            .collect(),
    }));
    for p in &b.plans {
        v.push(Block::Heading(format!(
            "Plan {} ({}, {})",
            p.plan_id, p.mode, p.status
        )));
        v.push(Block::Para(format!(
            "Planned attributable {} byte(s), added {}; recorded attributable {}, added {}. Created {}.",
            p.planned.attributable, p.planned.added, p.recorded.attributable_bytes, p.recorded.added_bytes, p.created_at_iso
        )));
        if !p.differences.is_empty() {
            v.push(Block::Bullets(
                p.differences.iter().map(|e| e.text.clone()).collect(),
            ));
        }
        if !p.remaining.is_empty() {
            v.push(Block::Para("Remaining actions:".into()));
            v.push(Block::Bullets(
                p.remaining
                    .iter()
                    .map(|r| format!("#{} {} ({}): {}", r.index, r.kind, r.state, r.text))
                    .collect(),
            ));
        }
    }
    ("removarr report".to_string(), v)
}

// ---------------------------------------------------------------- markdown

/// Escapes text for Markdown (inline and table cells): HTML-significant characters become
/// entities and Markdown punctuation is backslash-escaped. Newlines become spaces.
#[must_use]
pub fn md_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in sanitize(text).chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\\' | '`' | '*' | '_' | '[' | ']' | '|' | '#' | '!' | '(' | ')' | '~' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

#[must_use]
pub fn to_markdown(b: &ReportBundle) -> String {
    let (title, blocks) = document(b);
    let mut out = format!("# {}\n\n", md_escape(&title));
    for blk in blocks {
        match blk {
            Block::Heading(h) => out.push_str(&format!("## {}\n\n", md_escape(&h))),
            Block::Para(p) => out.push_str(&format!("{}\n\n", md_escape(&p))),
            Block::Bullets(items) => {
                for i in items {
                    out.push_str(&format!("- {}\n", md_escape(&i)));
                }
                out.push('\n');
            }
            Block::Table(t) => {
                out.push_str(&format!(
                    "| {} |\n",
                    t.headers
                        .iter()
                        .map(|h| md_escape(h))
                        .collect::<Vec<_>>()
                        .join(" | ")
                ));
                out.push_str(&format!("|{}|\n", vec![" --- "; t.headers.len()].join("|")));
                for r in &t.rows {
                    out.push_str(&format!(
                        "| {} |\n",
                        r.iter()
                            .map(|c| md_escape(c))
                            .collect::<Vec<_>>()
                            .join(" | ")
                    ));
                }
                out.push('\n');
            }
        }
    }
    out
}

// ---------------------------------------------------------------- html

#[must_use]
pub fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in sanitize(text).chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c if c.is_control() && c != '\n' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

#[must_use]
pub fn to_html(b: &ReportBundle) -> String {
    let (title, blocks) = document(b);
    let mut out = String::from(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'\">\
         <meta name=\"referrer\" content=\"no-referrer\">",
    );
    out.push_str(&format!("<title>{}</title>", html_escape(&title)));
    out.push_str(
        "<style>body{font:14px system-ui,sans-serif;margin:1rem auto;max-width:72rem;padding:0 1rem}\
         table{border-collapse:collapse;margin:0 0 1rem;display:block;overflow-x:auto}\
         th,td{border:1px solid #888;padding:.25rem .5rem;text-align:left}th{background:#eee}</style></head><body>",
    );
    out.push_str(&format!("<h1>{}</h1>\n", html_escape(&title)));
    for blk in blocks {
        match blk {
            Block::Heading(h) => out.push_str(&format!("<h2>{}</h2>\n", html_escape(&h))),
            Block::Para(p) => out.push_str(&format!("<p>{}</p>\n", html_escape(&p))),
            Block::Bullets(items) => {
                out.push_str("<ul>");
                for i in items {
                    out.push_str(&format!("<li>{}</li>", html_escape(&i)));
                }
                out.push_str("</ul>\n");
            }
            Block::Table(t) => {
                out.push_str("<table><thead><tr>");
                for h in &t.headers {
                    out.push_str(&format!("<th scope=\"col\">{}</th>", html_escape(h)));
                }
                out.push_str("</tr></thead><tbody>");
                for r in &t.rows {
                    out.push_str("<tr>");
                    for c in r {
                        out.push_str(&format!("<td>{}</td>", html_escape(c)));
                    }
                    out.push_str("</tr>");
                }
                out.push_str("</tbody></table>\n");
            }
        }
    }
    out.push_str("</body></html>\n");
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn csv_cells_neutralize_formulas_and_quote() {
        assert_eq!(csv_cell("=1+1"), "'=1+1");
        assert_eq!(csv_cell("+cmd"), "'+cmd");
        assert_eq!(csv_cell("-2"), "'-2");
        assert_eq!(csv_cell("@SUM(A1)"), "'@SUM(A1)");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        assert_eq!(csv_cell("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_cell("line1\nline2\r\nx"), "line1 line2  x");
        assert_eq!(csv_cell("plain"), "plain");
        assert!(!csv_cell("=HYPERLINK(\"http://evil\",\"x\")").contains("evil"));
        assert_eq!(csv_cell("=cmd|' /C calc'!A0"), "'=cmd|' /C calc'!A0");
    }

    #[test]
    fn html_escape_covers_markup_and_attributes() {
        assert_eq!(
            html_escape("<script>alert('x')</script> & \"q\""),
            "&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; &quot;q&quot;"
        );
        assert!(!html_escape("<img src=x onerror=1>").contains('<'));
    }

    #[test]
    fn markdown_escape_blocks_tables_links_and_html() {
        let s = md_escape("a|b [x](y) <b>`c`</b> *d*\nnext");
        assert!(!s.contains("a|b") && !s.contains('<') && !s.contains('\n'));
        assert!(s.contains("\\|") && s.contains("\\[") && s.contains("&lt;b&gt;"));
    }
}
