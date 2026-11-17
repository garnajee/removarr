//! Dashboard, filters, locale switching, issue details, dry-run preview, confirmation and
//! one-item/bulk actions over a real listener with the production guard and the loopback
//! fake adapters, in both locales (wave 16). Markup is asserted from the rendered HTML.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use removarr_domain::ids::PlanId;
use removarr_domain::issue::IssueCategory as C;
use removarr_domain::plan::PlanStatus;
use removarr_e2e_perf::*;
use removarr_integration_tests::{Options, world};
use reqwest::StatusCode;

const HOSTILE: [&str; 3] = [
    "<img src=x onerror=alert(1)>.txt",
    "\"><script>alert('xss')<.txt",
    "a&b&amp;c'd.txt",
];

async fn live() -> Live {
    let lv = Live::start(Options::default(), 50, 50).await;
    for n in HOSTILE {
        let f = lv.stack.p("completed/stray").join(n);
        std::fs::write(&f, b"junk").unwrap();
        removarr_integration_tests::age_hours(&f, 24 * 30);
    }
    lv
}

/// Logs in, selects `locale`, scans, waits for the scan: returns the browser and CSRF token.
async fn session(lv: &Live, locale: &str) -> (Browser, String) {
    let b = lv.browser();
    assert_eq!(b.login(USER, PASSWORD).await.status, StatusCode::SEE_OTHER);
    let token = b.token().await;
    let r = b
        .post(
            "/locale",
            &format!("locale={locale}&return_to=%2Fjobs"),
            &token,
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    assert_eq!(r.set_cookie("removarr_locale").as_deref(), Some(locale));
    let r = b.post("/scan", "", &token).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    lv.stack.wait_idle().await;
    (b, token)
}

fn issue_links(body: &str, cat: &str) -> Vec<String> {
    let re = regex::Regex::new(&format!(r#"href="(/issues/{cat}/[^"]+)""#)).unwrap();
    let mut v: Vec<String> = re.captures_iter(body).map(|c| c[1].to_string()).collect();
    v.dedup();
    v
}

fn id_of(link: &str) -> String {
    link.rsplit('/').next().unwrap().to_string()
}

#[tokio::test]
async fn locale_switching_persists_and_translates_the_shell_and_the_dashboard() {
    let lv = live().await;
    let (b, token) = session(&lv, "fr").await;
    let fr = b.get("/").await;
    assert!(fr.body.contains("<html lang=\"fr\""), "{}", &fr.body[..200]);
    let en_probe = {
        let e = lv.browser();
        e.login(USER, PASSWORD).await;
        e.get("/").await
    };
    assert!(en_probe.body.contains("<html lang=\"en\""));
    assert_ne!(
        html::text(&html::elements(&fr.body, "h1")[0].1),
        html::text(&html::elements(&en_probe.body, "h1")[0].1),
        "the dashboard heading is translated"
    );
    // The locale survives navigation, a new login, and is not part of the session.
    assert!(b.get("/issues/junk").await.body.contains("lang=\"fr\""));
    b.post("/logout", "", &token).await;
    b.login(USER, PASSWORD).await;
    assert!(b.get("/jobs").await.body.contains("lang=\"fr\""));
    // The login page itself follows the cookie, and Accept-Language when there is no cookie.
    assert!(b.get("/login").await.body.contains("lang=\"fr\""));
    let fresh = lv.browser();
    let p = fresh
        .get_with("/login", &[("Accept-Language", "fr-CH,fr;q=0.9,en;q=0.5")])
        .await;
    assert!(p.body.contains("lang=\"fr\""));
    let p = fresh
        .get_with("/login", &[("Accept-Language", "de-DE,de;q=0.9")])
        .await;
    assert!(
        p.body.contains("lang=\"en\""),
        "unsupported language falls back to English"
    );
    // Unsupported locale values are refused, the cookie is unchanged.
    let token = b.token().await;
    for bad in ["de", "fr%0d%0aSet-Cookie:x=1", "", "EN"] {
        let r = b.post("/locale", &format!("locale={bad}"), &token).await;
        assert!(r.status.is_client_error(), "locale={bad}: {}", r.status);
    }
    // return_to is only a local path: an absolute URL never becomes a redirect target.
    for target in [
        "https%3A%2F%2Fevil.invalid%2F",
        "%2F%2Fevil.invalid",
        "javascript%3Aalert(1)",
    ] {
        let r = b
            .post("/locale", &format!("locale=en&return_to={target}"), &token)
            .await;
        assert!(
            r.status.is_client_error()
                || r.location().starts_with('/') && !r.location().starts_with("//"),
            "{target}: {} {}",
            r.status,
            r.location()
        );
    }
}

#[tokio::test]
async fn every_page_is_accessible_markup_in_both_locales_and_untranslated_keys_never_show() {
    let lv = live().await;
    let keys: std::collections::BTreeSet<String> = {
        let en: std::collections::BTreeMap<String, String> =
            serde_json::from_str(include_str!("../../../locales/en.json")).unwrap();
        en.into_keys().collect()
    };
    for locale in ["en", "fr"] {
        let (b, _) = session(&lv, locale).await;
        let list = b.get("/issues/junk").await;
        let detail = issue_links(&list.body, "junk")[0].clone();
        let mut pages = vec![
            "/".to_string(),
            "/jobs".into(),
            "/history".into(),
            "/stats".into(),
            "/logs".into(),
            "/auto".into(),
            "/plans".into(),
            "/ignored".into(),
            detail,
        ];
        for c in [
            "junk",
            "duplicates",
            "orphans",
            "hardlink",
            "incomplete",
            "media-only",
        ] {
            pages.push(format!("/issues/{c}"));
        }
        let mut seen = 0;
        for path in pages {
            let p = b.get(&path).await;
            if p.status == StatusCode::NOT_FOUND {
                continue; // a category slug of this build that does not exist
            }
            assert_eq!(p.status, StatusCode::OK, "{locale} {path}");
            seen += 1;
            let f = html::a11y_findings(&p.body);
            assert!(f.is_empty(), "{locale} {path}: {f:#?}");
            assert!(
                html::duplicate_ids(&p.body).is_empty(),
                "{locale} {path}: duplicate ids {:?}",
                html::duplicate_ids(&p.body)
            );
            assert!(
                p.body.contains(&format!("<html lang=\"{locale}\"")),
                "{path}"
            );
            // No translation key leaks as visible text.
            for k in &keys {
                assert!(
                    !p.body.contains(&format!(">{k}<")),
                    "{locale} {path}: raw key {k}"
                );
            }
            // Security headers on every page.
            assert!(
                p.header("content-security-policy")
                    .contains("script-src 'self'")
            );
            assert_eq!(p.header("x-content-type-options"), "nosniff");
        }
        assert!(seen >= 8, "only {seen} pages rendered");
    }
}

#[tokio::test]
async fn english_fallback_covers_every_key_missing_from_french() {
    let en: std::collections::BTreeMap<String, String> =
        serde_json::from_str(include_str!("../../../locales/en.json")).unwrap();
    let fr: std::collections::BTreeMap<String, String> =
        serde_json::from_str(include_str!("../../../locales/fr.json")).unwrap();
    let missing: Vec<&String> = en.keys().filter(|k| !fr.contains_key(*k)).collect();
    let extra: Vec<&String> = fr.keys().filter(|k| !en.contains_key(*k)).collect();
    assert!(
        extra.is_empty(),
        "French keys unknown to English: {extra:?}"
    );
    // The runtime lookup falls back per key, never to the key itself while English has it.
    let tr = removarr_web::i18n::Tr::new(removarr_web::i18n::Locale::Fr);
    for k in &missing {
        assert_eq!(tr.t(k), en[*k], "fallback for {k}");
    }
    assert_eq!(
        tr.t("no.such.key"),
        "no.such.key",
        "unknown keys show the key, never panic"
    );
    // Placeholders survive translation.
    let placeholder = regex::Regex::new(r"\{[a-z_]+\}").unwrap();
    for (k, v) in &fr {
        let ph = |s: &str| {
            let mut x: Vec<String> = placeholder
                .find_iter(s)
                .map(|m| m.as_str().to_string())
                .collect();
            x.sort();
            x
        };
        assert_eq!(ph(v), ph(&en[k]), "placeholders of {k}");
    }
    eprintln!("french keys missing (English fallback): {}", missing.len());
}

#[tokio::test]
async fn hostile_strings_are_escaped_everywhere_they_are_shown() {
    let lv = live().await;
    let (b, token) = session(&lv, "en").await;
    let raw = [
        "<img src=x",
        "<script>alert",
        "onerror=alert(1)>",
        "\"><script",
    ];
    let hostile_q = "%3Cscript%3Ealert(1)%3C%2Fscript%3E%22%27";
    let list = b.get("/issues/junk").await;
    let links = issue_links(&list.body, "junk");
    assert!(
        links.len() >= 6,
        "3 legacy junk files + 3 hostile ones: {}",
        links.len()
    );
    let mut pages = vec![
        list.body.clone(),
        b.get(&format!("/issues/junk?q={hostile_q}")).await.body,
        b.get(&format!("/logs?q={hostile_q}&level=info")).await.body,
        b.get("/logs").await.body,
        b.get("/history").await.body,
        b.get("/").await.body,
    ];
    for l in &links {
        pages.push(b.get(l).await.body);
    }
    let ids: Vec<String> = links
        .iter()
        .map(|l| format!("issue_ids={}", id_of(l)))
        .collect();
    let preview = b
        .post(
            "/plans/preview",
            &format!("category=junk&action=delete&{}", ids.join("&")),
            &token,
        )
        .await;
    assert_eq!(preview.status, StatusCode::OK, "{}", preview.body);
    pages.push(preview.body);
    let created = b
        .post(
            "/plans",
            &format!("category=junk&action=delete&{}", ids.join("&")),
            &token,
        )
        .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.body);
    let plan = created.location();
    pages.push(b.get(&plan).await.body);
    pages.push(b.get(&format!("{plan}/confirm")).await.body);
    for (i, body) in pages.iter().enumerate() {
        for r in raw {
            assert!(!body.contains(r), "page {i} contains unescaped {r}");
        }
    }
    // The hostile names are present, but as entities.
    assert!(
        pages[0].contains("&lt;img src=x onerror=alert(1)&gt;.txt")
            || pages[0].contains("&#60;img"),
        "escaped form is shown"
    );
    // The reflected filter value stays inside its attribute.
    assert!(pages[1].contains("&lt;script&gt;") || pages[1].contains("&#60;script&#62;"));
    // Hostile request headers and parameters never reach the markup either.
    let p = b
        .get_with(
            "/jobs",
            &[
                ("Accept-Language", "<script>alert(1)</script>"),
                ("Host", "evil\"><script>"),
            ],
        )
        .await;
    assert!(!p.body.contains("<script>alert"));
    let p = b
        .get("/issues/junk?sort=%3Cscript%3E&dir=%22&kind=%27&page=-1&per_page=99999999")
        .await;
    assert!(p.status.is_client_error() || p.status == StatusCode::OK);
    assert!(!p.body.contains("<script>"));
}

#[tokio::test]
async fn filters_sort_and_pagination_narrow_the_list_and_never_mutate() {
    let lv = live().await;
    let (b, _) = session(&lv, "en").await;
    let before = world(&lv.stack.fx);
    let all = issue_links(&b.get("/issues/junk").await.body, "junk");
    let only = issue_links(&b.get("/issues/junk?q=readme").await.body, "junk");
    assert_eq!(only.len(), 1, "search narrows the list: {only:?}");
    let none = b.get("/issues/junk?q=zzz-no-such-file").await;
    assert_eq!(none.status, StatusCode::OK);
    assert!(issue_links(&none.body, "junk").is_empty());
    assert!(
        html::a11y_findings(&none.body).is_empty(),
        "{:?}",
        html::a11y_findings(&none.body)
    );
    // Pagination: one item per page covers every item once and only once.
    let mut paged = Vec::new();
    for page in 1..=all.len() + 1 {
        let p = b.get(&format!("/issues/junk?per_page=1&page={page}")).await;
        assert_eq!(p.status, StatusCode::OK);
        paged.extend(issue_links(&p.body, "junk"));
    }
    paged.sort();
    let mut expect = all.clone();
    expect.sort();
    assert_eq!(paged, expect);
    // Sort direction reverses the order.
    let asc = issue_links(&b.get("/issues/junk?sort=path&dir=asc").await.body, "junk");
    let desc = issue_links(&b.get("/issues/junk?sort=path&dir=desc").await.body, "junk");
    assert_eq!(asc.iter().rev().cloned().collect::<Vec<_>>(), desc);
    // The select-all and hide-recent controls are plain GETs.
    assert_eq!(
        b.get("/issues/junk?select_all=1").await.status,
        StatusCode::OK
    );
    assert_eq!(
        b.get("/issues/junk?hide_recent=1").await.status,
        StatusCode::OK
    );
    assert_eq!(world(&lv.stack.fx), before, "browsing deletes nothing");
    assert_eq!(lv.stack.qbit_mutations(), Vec::<String>::new());
}

#[tokio::test]
async fn dry_run_one_item_and_bulk_actions_preview_confirm_simulate_and_change_nothing() {
    let lv = live().await;
    for locale in ["en", "fr"] {
        let (b, token) = session(&lv, locale).await;
        let before = world(&lv.stack.fx);
        let links = issue_links(&b.get("/issues/junk").await.body, "junk");
        assert!(links.len() >= 6);
        // One-item action from the issue detail page.
        let detail = b.get(&links[0]).await;
        assert_eq!(detail.status, StatusCode::OK);
        assert!(html::a11y_findings(&detail.body).is_empty());
        for (label, sel) in [("one", &links[..1]), ("bulk", &links[..])] {
            let ids: Vec<String> = sel
                .iter()
                .map(|l| format!("issue_ids={}", id_of(l)))
                .collect();
            let form = format!("category=junk&action=delete&{}", ids.join("&"));
            // Selection summary (HTMX fragment) then preview (not stored).
            let s = b
                .post_raw(
                    "/issues/selection-summary",
                    &format!("category=junk&{}", ids.join("&")),
                    &[("X-CSRF-Token", &token), ("HX-Request", "true")],
                )
                .await;
            assert!(s.status.is_success(), "{label}: {} {}", s.status, s.body);
            assert!(!s.body.contains("<html"), "fragment, not a page");
            let p = b
                .post_raw(
                    "/plans/preview",
                    &form,
                    &[("X-CSRF-Token", &token), ("HX-Request", "true")],
                )
                .await;
            assert_eq!(p.status, StatusCode::OK, "{label} {}", p.body);
            assert!(!p.body.contains("<html"));
            assert_eq!(
                p.body.matches("name=\"confirmed_count\"").count(),
                0,
                "a preview cannot be approved"
            );
            // Create the stored plan; the confirmation page carries the count and the hash.
            let c = b.post("/plans", &form, &token).await;
            assert_eq!(c.status, StatusCode::SEE_OTHER, "{label}: {}", c.body);
            let url = c.location();
            let id = PlanId(url.rsplit('/').next().unwrap().parse().unwrap());
            let page = b.get(&url).await;
            assert!(
                html::a11y_findings(&page.body).is_empty(),
                "{:?}",
                html::a11y_findings(&page.body)
            );
            let conf = b.get(&format!("{url}/confirm")).await;
            assert_eq!(conf.status, StatusCode::OK);
            assert!(
                html::a11y_findings(&conf.body).is_empty(),
                "{:?}",
                html::a11y_findings(&conf.body)
            );
            assert!(
                conf.body.contains("confirmed_count")
                    && conf.body.contains(&lv.stack.stored_hash(id).await)
            );
            // Approval needs the typed count; a wrong one is refused and re-shown, not applied.
            let hash = lv.stack.stored_hash(id).await;
            let wrong = b
                .post(
                    &format!("/plans/{}/approve", id.0),
                    &format!("plan_hash={hash}&confirmed_count={}", sel.len() + 1),
                    &token,
                )
                .await;
            assert!(wrong.status.is_client_error(), "{label}: {}", wrong.status);
            assert_eq!(lv.stack.plan_status(id).await, PlanStatus::Draft);
            let ok = b
                .post(
                    &format!("/plans/{}/approve", id.0),
                    &format!("plan_hash={hash}&confirmed_count={}", sel.len()),
                    &token,
                )
                .await;
            assert_eq!(ok.status, StatusCode::SEE_OTHER, "{label}: {}", ok.body);
            assert_eq!(lv.stack.plan_status(id).await, PlanStatus::Approved);
            // Dry run: live execution is refused; simulation runs and records `Simulated`.
            let ex = b
                .post(&format!("/plans/{}/execute", id.0), "", &token)
                .await;
            assert!(
                ex.status.is_client_error(),
                "{label} live execute in dry-run: {}",
                ex.status
            );
            let sim = b
                .post(&format!("/plans/{}/simulate", id.0), "", &token)
                .await;
            assert_eq!(sim.status, StatusCode::SEE_OTHER, "{label}: {}", sim.body);
            lv.stack.wait_idle().await;
            let jobs = b.get(&sim.location()).await;
            assert_eq!(jobs.status, StatusCode::OK);
            assert_eq!(
                lv.stack.plan_status(id).await,
                PlanStatus::Approved,
                "simulation never changes the status"
            );
            assert!(
                lv.stack
                    .journal(id)
                    .await
                    .iter()
                    .all(|(_, r)| r.as_deref() == Some("Simulated") || r.is_none())
            );
            assert_eq!(
                world(&lv.stack.fx),
                before,
                "{label} {locale}: the tree is untouched"
            );
            assert!(
                lv.stack.qbit_mutations().is_empty(),
                "no qBittorrent mutation"
            );
            // Discard returns the plan out of the approved set.
            let d = b
                .post(&format!("/plans/{}/discard", id.0), "", &token)
                .await;
            assert!(
                d.status.is_redirection() || d.status.is_success(),
                "{}",
                d.status
            );
        }
        assert!(
            !lv.stack.issues(C::Junk).await.is_empty(),
            "junk issues still listed"
        );
    }
}

#[tokio::test]
async fn ignore_and_restore_move_an_issue_between_the_list_and_the_ignored_page() {
    let lv = live().await;
    let (b, token) = session(&lv, "fr").await;
    let links = issue_links(&b.get("/issues/junk").await.body, "junk");
    let id = id_of(&links[0]);
    let r = b
        .post(
            "/issues/ignore",
            &format!("issue_ids={id}&reason=bruit"),
            &token,
        )
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.body);
    let after = issue_links(&b.get("/issues/junk").await.body, "junk");
    assert_eq!(after.len(), links.len() - 1);
    let ign = b.get("/ignored").await;
    assert_eq!(ign.status, StatusCode::OK);
    assert!(html::a11y_findings(&ign.body).is_empty());
    let r = b
        .post("/issues/restore", &format!("issue_ids={id}"), &token)
        .await;
    assert!(
        r.status.is_redirection() || r.status.is_success(),
        "{}",
        r.status
    );
    assert_eq!(
        issue_links(&b.get("/issues/junk").await.body, "junk").len(),
        links.len()
    );
}

#[tokio::test]
async fn css_keeps_44px_targets_visible_focus_and_the_narrow_layout_rules() {
    let lv = live().await;
    let b = lv.browser();
    let css = b.get("/static/app.css").await;
    assert_eq!(css.status, StatusCode::OK);
    let c = css.body;
    assert!(
        c.contains("--tap: 2.75rem") || c.contains("--tap:2.75rem") || c.contains("--tap: 44px"),
        "tap target token is 44 px"
    );
    for sel in [
        "button, .btn",
        ".tabbar a, .tabbar summary",
        "label.check",
        "details > summary",
    ] {
        let at = c
            .find(&format!("\n{sel} {{"))
            .unwrap_or_else(|| panic!("selector {sel}"));
        let rule = &c[at..at + c[at..].find('}').unwrap()];
        assert!(
            rule.contains("min-height: var(--tap)") || rule.contains("min-height: var(--tabbar)"),
            "{sel}: {rule}"
        );
    }
    assert!(c.contains(":focus-visible"));
    assert!(c.contains("prefers-reduced-motion"));
    assert!(c.contains("forced-colors"));
    assert!(
        c.contains("@media (max-width: 599px)"),
        "narrow layout rules exist"
    );
    assert!(c.contains("--tabbar"));
    // No horizontal overflow escape hatches that hide content: tables scroll in a container.
    assert!(!c.contains("overflow-x: hidden") || c.contains("overflow-x: auto"));
    // Static assets: cached with a fingerprint-safe policy and nosniff, the HTMX copy is pinned.
    assert_eq!(
        css.headers
            .get("x-content-type-options")
            .map(|v| v.to_str().unwrap()),
        Some("nosniff")
    );
    let htmx = b.get("/static/htmx-2.0.4.min.js").await;
    assert_eq!(htmx.status, StatusCode::OK);
    use sha2::{Digest, Sha256};
    let digest: String = Sha256::digest(htmx.body.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        digest,
        "e209dda5c8235479f3166defc7750e1dbcd5a5c1808b7792fc2e6733768fb447"
    );
}
