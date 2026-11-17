//! Media search and the manual grab request: read-only search, opaque tokens, form availability
//! and the plan path to the confirmation page.

use super::followup::get_as;
use super::*;
use crate::i18n::{Locale, Tr};
use removarr_domain::arr::{ArrMediaRef, ArrService, ReleaseRef, ReleaseSearchResult};

const TOKEN: &str = "rel-0123456789abcdef0123456789abcdef";
const RAW_GUID: &str = "https://tracker.example/download?passkey=SECRETPASSKEY";

fn result(token: &str, approved: bool) -> ReleaseSearchResult {
    ReleaseSearchResult {
        release: ReleaseRef {
            guid: token.to_string(),
            indexer_id: 2,
        },
        indexer: "Indexer One".into(),
        title: "Synthetic.Movie.2021.1080p".into(),
        size: 4_000_000_000,
        seeders: Some(12),
        leechers: Some(1),
        age_days: 3,
        protocol: "torrent".into(),
        quality: "Bluray-1080p".into(),
        custom_formats: vec!["CF-Beta".into()],
        cf_score: 50,
        approved,
        rejections: if approved {
            vec![]
        } else {
            vec!["Quality CAM is not wanted".into()]
        },
        download_allowed: approved,
    }
}

fn world(arr_grab: bool, dry_run: bool) -> World {
    let w = World::build(Arc::new(AllowGuard), false, dry_run, arr_grab);
    *w.search.results.lock().unwrap() = vec![
        result(TOKEN, true),
        result("rel-fedcba9876543210fedcba9876543210", false),
    ];
    w
}

const SEARCH: &str = "/media/radarr/7/search";
const GRAB: &str = "/media/radarr/7/grab-plan";

#[tokio::test]
async fn search_renders_tokens_only_and_sends_nothing_but_the_search() {
    let w = world(true, false);
    let (s, _, body) = w.post(SEARCH, "").await;
    assert_eq!(s, StatusCode::OK);
    assert!(body.contains(TOKEN));
    assert!(body.contains("Indexer One") && body.contains("Quality CAM is not wanted"));
    assert!(body.contains("CF-Beta") && body.contains(">50<"));
    assert!(
        !body.contains(RAW_GUID) && !body.contains("passkey") && !body.contains("tracker.example")
    );
    assert_eq!(w.search.calls.lock().unwrap().len(), 1);
    assert_eq!(w.mutations(), 0, "a search creates no plan, job or ignore");
    assert!(w.plans.created.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_entry_page_never_searches_and_a_sonarr_episode_is_forwarded() {
    let w = world(true, false);
    let (s, _, body) = w.get("/media/sonarr/12?episode_id=345").await;
    assert_eq!(s, StatusCode::OK);
    assert!(w.search.calls.lock().unwrap().is_empty());
    assert!(body.contains("/media/sonarr/12/search"));
    let (s, ..) = w.post("/media/sonarr/12/search", "episode_id=345").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        w.search.calls.lock().unwrap()[0],
        ArrMediaRef {
            service: ArrService::Sonarr,
            media_id: 12,
            episode_ids: vec![345]
        }
    );
}

#[tokio::test]
async fn the_grab_form_is_only_offered_for_a_grabbable_release_when_enabled_and_live() {
    let (_, _, on) = world(true, false).post(SEARCH, "").await;
    assert_eq!(on.matches(GRAB).count(), 1, "only the approved release");
    assert!(on.contains("name=\"csrf_token\""));

    for (grab, dry, key) in [
        (false, false, "media.grab_off_capability"),
        (true, true, "media.grab_off_dry_run"),
    ] {
        let w = world(grab, dry);
        let (s, _, body) = w.post(SEARCH, "").await;
        assert_eq!(s, StatusCode::OK);
        assert!(!body.contains(GRAB), "no form when {key}");
        for l in [Locale::En, Locale::Fr] {
            let tr = Tr::new(l);
            let text = tr.t(key).replace('\'', "&#39;");
            let (_, _, page) = get_as(&w, "/media/radarr/7", l.as_str(), false).await;
            assert!(page.contains(&text), "{key} {l:?}");
        }
    }
}

#[tokio::test]
async fn a_grab_plan_from_a_search_goes_to_the_confirmation_page_and_grabs_nothing() {
    let w = world(true, false);
    let (s, h, _) = w.post(GRAB, &format!("release={TOKEN}&indexer_id=2")).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(
        h[header::LOCATION],
        "/plans/00000000-0000-0000-0000-00000000002a/confirm"
    );
    let created = w.plans.created.lock().unwrap().clone();
    assert_eq!(
        created[0],
        PlanRequest::Release {
            media: ArrMediaRef {
                service: ArrService::Radarr,
                media_id: 7,
                episode_ids: vec![]
            },
            release: ReleaseRef {
                guid: TOKEN.into(),
                indexer_id: 2
            }
        }
    );
    assert!(w.jobs.mutations() == 0, "no execution is queued");
    let (s, _, page) = w
        .get("/plans/00000000-0000-0000-0000-00000000002a/confirm")
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(page.contains("name=\"plan_hash\""));
}

#[tokio::test]
async fn media_requests_are_validated() {
    let w = world(true, false);
    for (uri, form) in [
        (
            GRAB,
            "release=0123456789abcdef0123456789abcdef&indexer_id=2",
        ),
        (GRAB, "release=rel-ZZZ&indexer_id=2"),
        (GRAB, &format!("release={RAW_GUID}&indexer_id=2")),
        (GRAB, &format!("release={TOKEN}&indexer_id=-1")),
        (GRAB, &format!("release={TOKEN}")),
        (GRAB, ""),
        (GRAB, &format!("release={TOKEN}&indexer_id=2&episode_id=5")),
        (
            "/media/radarr/0/grab-plan",
            &format!("release={TOKEN}&indexer_id=2"),
        ),
        ("/media/radarr/x/search", ""),
        ("/media/lidarr/7/search", ""),
        (SEARCH, "episode_id=5"),
    ] {
        let (s, _, body) = w.post(uri, form).await;
        assert!(
            s == StatusCode::UNPROCESSABLE_ENTITY || s == StatusCode::NOT_FOUND,
            "{uri} {form}: {s}"
        );
        assert!(!body.contains("SECRETPASSKEY"));
    }
    assert!(w.plans.created.lock().unwrap().is_empty());
    assert!(w.search.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn media_commands_need_the_command_guard_and_pages_never_mutate() {
    let w = World::new(Arc::new(PageOnlyGuard));
    let (s, ..) = w.post(SEARCH, "").await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, ..) = w.post(GRAB, &format!("release={TOKEN}&indexer_id=2")).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, ..) = w.get("/media/radarr/7").await;
    assert_eq!(s, StatusCode::OK);
    assert!(w.search.calls.lock().unwrap().is_empty());
    assert_eq!(w.mutations(), 0);
}
