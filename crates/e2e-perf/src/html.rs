//! Markup assertions over server-rendered HTML (regex based: the templates are our own and
//! well formed; this is not a general HTML parser).

use regex::Regex;

/// The first hidden `csrf_token` value of a page.
#[must_use]
pub fn csrf(body: &str) -> String {
    let m = "name=\"csrf_token\" value=\"";
    let a = body
        .find(m)
        .unwrap_or_else(|| panic!("no csrf field in: {}", &body[..body.len().min(300)]))
        + m.len();
    body[a..a + body[a..].find('"').unwrap()].to_string()
}

/// Every opening tag `<name ...>` as its attribute text.
#[must_use]
pub fn tags(body: &str, name: &str) -> Vec<String> {
    let re = Regex::new(&format!(r"(?is)<{name}(\s[^>]*)?>")).unwrap();
    re.captures_iter(body)
        .map(|c| c.get(1).map_or(String::new(), |m| m.as_str().to_string()))
        .collect()
}

/// The value of attribute `attr` in an attribute text.
#[must_use]
pub fn attr(tag_attrs: &str, attr: &str) -> Option<String> {
    let re = Regex::new(&format!(r#"(?is)(?:^|\s){attr}\s*=\s*"([^"]*)""#)).unwrap();
    re.captures(tag_attrs).map(|c| c[1].to_string())
}

#[must_use]
pub fn has_flag(tag_attrs: &str, flag: &str) -> bool {
    let re = Regex::new(&format!(r"(?is)(?:^|\s){flag}(?:\s|=|$)")).unwrap();
    re.is_match(tag_attrs)
}

/// Text between `<name ...>` and `</name>` for every element (non nested use only).
#[must_use]
pub fn elements(body: &str, name: &str) -> Vec<(String, String)> {
    let re = Regex::new(&format!(r"(?is)<{name}(\s[^>]*)?>(.*?)</{name}>")).unwrap();
    re.captures_iter(body)
        .map(|c| {
            (
                c.get(1).map_or(String::new(), |m| m.as_str().to_string()),
                c[2].to_string(),
            )
        })
        .collect()
}

/// Visible text of a fragment: tags removed, `aria-label`s of inner elements excluded.
#[must_use]
pub fn text(fragment: &str) -> String {
    let re = Regex::new(r"(?is)<[^>]*>").unwrap();
    re.replace_all(fragment, " ").trim().to_string()
}

/// Accessibility findings of one full page (empty when the page passes). The checks are the
/// markup half of the UX contract: language, landmarks, one heading level 1, skip link target,
/// labelled controls, named buttons and links, alternative text, status not by colour alone.
#[must_use]
pub fn a11y_findings(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let html = tags(body, "html");
    match html.first().and_then(|h| attr(h, "lang")) {
        Some(l) if l == "en" || l == "fr" => {}
        other => out.push(format!("html lang missing or unexpected: {other:?}")),
    }
    if !tags(body, "meta")
        .iter()
        .any(|m| attr(m, "name").as_deref() == Some("viewport"))
    {
        out.push("no viewport meta".into());
    }
    if elements(body, "title")
        .first()
        .is_none_or(|(_, t)| t.trim().is_empty())
    {
        out.push("empty title".into());
    }
    let mains = tags(body, "main");
    if mains.len() != 1 || attr(&mains[0], "id").as_deref() != Some("main") {
        out.push(format!("expected one <main id=main>, got {}", mains.len()));
    }
    if !body.contains("href=\"#main\"") {
        out.push("no skip link to #main".into());
    }
    let navs = tags(body, "nav");
    if navs.is_empty() || navs.iter().any(|n| attr(n, "aria-label").is_none()) {
        out.push("nav landmark missing or unlabelled".into());
    }
    let h1 = tags(body, "h1").len();
    if h1 != 1 {
        out.push(format!("expected one h1, got {h1}"));
    }
    // Labelled controls.
    let ids: Vec<String> = tags(body, "label")
        .iter()
        .filter_map(|l| attr(l, "for"))
        .collect();
    for kind in ["input", "select", "textarea"] {
        for t in tags(body, kind) {
            let ty = attr(&t, "type").unwrap_or_default();
            if ty == "hidden" || ty == "submit" {
                continue;
            }
            let named = attr(&t, "aria-label").is_some()
                || attr(&t, "aria-labelledby").is_some()
                || attr(&t, "id").is_some_and(|id| ids.contains(&id));
            // A checkbox wrapped in <label class="check"> is labelled by containment.
            let wrapped = ty == "checkbox" && body.contains("<label");
            if !named && !wrapped {
                out.push(format!("unlabelled <{kind}{t}>"));
            }
        }
    }
    for (a, inner) in elements(body, "button") {
        if text(&inner).is_empty() && attr(&a, "aria-label").is_none() {
            out.push("button without an accessible name".into());
        }
    }
    for (a, inner) in elements(body, "a") {
        if text(&inner).is_empty() && attr(&a, "aria-label").is_none() {
            out.push(format!("link without an accessible name:{a}"));
        }
    }
    for t in tags(body, "img") {
        if attr(&t, "alt").is_none() {
            out.push("img without alt".into());
        }
    }
    // Decorative SVG must be hidden from assistive technology.
    for t in tags(body, "svg") {
        if attr(&t, "aria-hidden").as_deref() != Some("true") && attr(&t, "role").is_none() {
            out.push(format!("svg neither hidden nor labelled:{t}"));
        }
    }
    // Status never by colour alone: every badge has visible text.
    for (a, inner) in elements(body, "span") {
        if attr(&a, "class").is_some_and(|c| c.split(' ').any(|w| w == "badge"))
            && text(&inner).is_empty()
        {
            out.push("badge without text (colour only)".into());
        }
    }
    // Inline script, style and event handlers are forbidden by the CSP.
    for t in tags(body, "script") {
        if attr(&t, "src").is_none() {
            out.push("inline script".into());
        }
    }
    if Regex::new(r#"(?is)<[a-z][^>]*\s(style|on[a-z]+)\s*="#)
        .unwrap()
        .is_match(body)
    {
        out.push("inline style or event handler attribute".into());
    }
    out
}

/// Duplicate `id` attributes in a page.
#[must_use]
pub fn duplicate_ids(body: &str) -> Vec<String> {
    let re = Regex::new(r#"(?is)<[a-z0-9]+\s[^>]*?\sid\s*=\s*"([^"]+)""#).unwrap();
    let mut seen = std::collections::BTreeSet::new();
    let mut dup = Vec::new();
    for c in re.captures_iter(body) {
        if !seen.insert(c[1].to_string()) {
            dup.push(c[1].to_string());
        }
    }
    dup
}
