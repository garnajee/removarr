//! Request inputs and their validation. Every query and form field arrives as text and is
//! validated here, so a bad value always produces the same 422 shape (field name and a static
//! problem), never a parser message. One-item and bulk selections share [`issue_ids`].

use removarr_domain::arr::{ArrMediaRef, ArrService, ReleaseRef};
use removarr_domain::ids::{IssueId, JobId, PlanId, RawUuid, SessionId, SubjectKey};
use removarr_domain::issue::IssueCategory;
use removarr_domain::path::RootedPath;
use removarr_domain::plan::PlanHash;
use removarr_domain::session::ServiceKind;
use removarr_reporting::{LogLevel, StatsRange};
use removarr_workflows::plans::IssueAction;
use serde::Deserialize;

use crate::error::WebError;

/// Largest selection accepted in one request (bulk = unit, same path).
pub const MAX_SELECTION: usize = 2000;
pub const DEFAULT_PER_PAGE: u32 = 50;
pub const MAX_PER_PAGE: u32 = 200;
pub const MAX_PAGE: u32 = 10_000;
pub const MAX_SEARCH_CHARS: usize = 200;
pub const MAX_REASON_CHARS: usize = 500;
pub const MAX_SUBJECT_CHARS: usize = 2048;
/// Request bodies above this size are refused with 413 (architecture 12.3).
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Request targets (path and query) above this size are refused with 414.
pub const MAX_URI_BYTES: usize = 8192;

type V<T> = Result<T, WebError>;

fn hex_of_len(s: &str, n: usize) -> bool {
    s.len() == n
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn uuid(s: &str, field: &'static str) -> V<RawUuid> {
    if s.len() != 36 {
        return Err(WebError::invalid(field, "not a valid id"));
    }
    RawUuid::parse_str(s).map_err(|_| WebError::invalid(field, "not a valid id"))
}

/// Issue ids are the first 32 lowercase hex characters of a SHA-256.
pub fn issue_id(s: &str) -> V<IssueId> {
    if hex_of_len(s, 32) {
        Ok(IssueId(s.to_string()))
    } else {
        Err(WebError::invalid("issue_id", "not a valid issue id"))
    }
}

/// The selection of one or many issues. There is no implicit selection: empty is refused,
/// every id must be valid, duplicates are removed (order never matters).
pub fn issue_ids(raw: &[String]) -> V<Vec<IssueId>> {
    if raw.is_empty() {
        return Err(WebError::invalid("issue_ids", "select at least one item"));
    }
    if raw.len() > MAX_SELECTION {
        return Err(WebError::invalid("issue_ids", "too many items selected"));
    }
    let mut out = raw
        .iter()
        .map(|s| issue_id(s.trim()).map_err(|_| WebError::invalid("issue_ids", "invalid id")))
        .collect::<V<Vec<_>>>()?;
    out.sort();
    out.dedup();
    Ok(out)
}

pub fn plan_id(s: &str) -> V<PlanId> {
    uuid(s, "plan_id").map(PlanId::new)
}

pub fn job_id(s: &str) -> V<JobId> {
    uuid(s, "job_id").map(JobId::new)
}

/// `latest` or a session id.
pub fn session(s: &str) -> V<Option<SessionId>> {
    if s == "latest" {
        return Ok(None);
    }
    uuid(s, "session").map(|u| Some(SessionId::new(u)))
}

pub fn category(s: &str) -> V<IssueCategory> {
    IssueCategory::parse(s).map_err(|_| WebError::invalid("category", "unknown category"))
}

pub fn service(s: &str) -> V<ServiceKind> {
    match s {
        "qbittorrent" => Ok(ServiceKind::Qbittorrent),
        "sonarr" => Ok(ServiceKind::Sonarr),
        "radarr" => Ok(ServiceKind::Radarr),
        "jellyfin" => Ok(ServiceKind::Jellyfin),
        _ => Err(WebError::invalid("service", "unknown service")),
    }
}

pub fn plan_hash(s: &str) -> V<PlanHash> {
    if hex_of_len(s, 64) {
        Ok(PlanHash(s.to_string()))
    } else {
        Err(WebError::invalid("plan_hash", "not a valid plan hash"))
    }
}

/// Free text (search, reason): trimmed, bounded, no control characters. Empty is `None`.
pub fn text(raw: Option<&str>, field: &'static str, max_chars: usize) -> V<Option<String>> {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if s.chars().count() > max_chars {
        return Err(WebError::invalid(field, "too long"));
    }
    if s.chars().any(char::is_control) {
        return Err(WebError::invalid(
            field,
            "control characters are not allowed",
        ));
    }
    Ok(Some(s.to_string()))
}

/// Subject keys of the ignore list (as listed by the issue views).
pub fn subjects(raw: &[String]) -> V<Vec<SubjectKey>> {
    if raw.is_empty() {
        return Err(WebError::invalid("subjects", "select at least one item"));
    }
    if raw.len() > MAX_SELECTION {
        return Err(WebError::invalid("subjects", "too many items selected"));
    }
    let mut out = Vec::with_capacity(raw.len());
    for s in raw {
        let s = s.trim();
        if s.is_empty()
            || s.chars().count() > MAX_SUBJECT_CHARS
            || s.chars().any(char::is_control)
            || !s.contains(':')
        {
            return Err(WebError::invalid("subjects", "invalid subject"));
        }
        out.push(SubjectKey(s.to_string()));
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// `root:relative/path` as displayed by the plan preview.
pub fn rooted_paths(raw: &[String], field: &'static str) -> V<Vec<RootedPath>> {
    if raw.len() > MAX_SELECTION {
        return Err(WebError::invalid(field, "too many items"));
    }
    raw.iter()
        .map(|s| {
            let (root, rel) = s
                .split_once(':')
                .ok_or_else(|| WebError::invalid(field, "invalid path"))?;
            RootedPath::parse(root, rel).map_err(|_| WebError::invalid(field, "invalid path"))
        })
        .collect()
}

fn count(raw: Option<&str>, field: &'static str) -> V<u32> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(0),
        Some(s) if s.len() <= 9 && s.bytes().all(|b| b.is_ascii_digit()) => s
            .parse()
            .map_err(|_| WebError::invalid(field, "not a number")),
        Some(_) => Err(WebError::invalid(field, "not a number")),
    }
}

fn flag(raw: Option<&str>, field: &'static str) -> V<bool> {
    match raw.map(str::trim) {
        None | Some("" | "0" | "false" | "off") => Ok(false),
        Some("1" | "true" | "on") => Ok(true),
        Some(_) => Err(WebError::invalid(field, "not a boolean")),
    }
}

/// `consolidate`, `delete`, `delete_replaced`, `cleanup_stalled`, `fix_mode`,
/// `replace_release`, `remove_queue_item`.
pub fn issue_action(s: &str, delete_old_media: bool) -> V<IssueAction> {
    Ok(match s {
        "consolidate" => IssueAction::Consolidate,
        "delete" => IssueAction::Delete,
        "delete_replaced" => IssueAction::DeleteReplaced,
        "cleanup_stalled" => IssueAction::CleanupStalled,
        "fix_mode" => IssueAction::FixMode,
        "replace_release" => IssueAction::ReplaceRelease { delete_old_media },
        "remove_queue_item" => IssueAction::RemoveQueueItem,
        _ => return Err(WebError::invalid("action", "unknown action")),
    })
}

/// Form value of an action (inverse of [`issue_action`]).
#[must_use]
pub fn action_value(a: IssueAction) -> &'static str {
    match a {
        IssueAction::Consolidate => "consolidate",
        IssueAction::Delete => "delete",
        IssueAction::DeleteReplaced => "delete_replaced",
        IssueAction::CleanupStalled => "cleanup_stalled",
        IssueAction::FixMode => "fix_mode",
        IssueAction::ReplaceRelease { .. } => "replace_release",
        IssueAction::RemoveQueueItem => "remove_queue_item",
    }
}

// ---- issue list query ------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Age,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KindFilter {
    All,
    Folder,
    File,
}

/// Raw query of `GET /issues/{category}` and `GET /ignored`.
#[derive(Debug, Default, Deserialize)]
pub struct ListQueryRaw {
    pub q: Option<String>,
    pub sort: Option<String>,
    pub dir: Option<String>,
    pub hide_recent: Option<String>,
    pub kind: Option<String>,
    pub page: Option<String>,
    pub per_page: Option<String>,
}

/// Validated list query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListQuery {
    pub q: Option<String>,
    pub sort: SortKey,
    pub desc: bool,
    pub hide_recent: bool,
    pub kind: KindFilter,
    pub page: u32,
    pub per_page: u32,
}

fn bounded(raw: Option<&str>, field: &'static str, default: u32, min: u32, max: u32) -> V<u32> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(default),
        Some(s) => {
            let n = count(Some(s), field)?;
            if (min..=max).contains(&n) {
                Ok(n)
            } else {
                Err(WebError::invalid(field, "out of range"))
            }
        }
    }
}

impl ListQueryRaw {
    /// `hide_recent` defaults to on when the configured threshold is above zero.
    pub fn validate(&self, hide_recent_default: bool) -> V<ListQuery> {
        let sort = match self.sort.as_deref().map(str::trim) {
            None | Some("" | "size") => SortKey::Size,
            Some("name") => SortKey::Name,
            Some("age") => SortKey::Age,
            Some(_) => return Err(WebError::invalid("sort", "use name, size or age")),
        };
        let desc = match self.dir.as_deref().map(str::trim) {
            None | Some("") => sort != SortKey::Name,
            Some("asc") => false,
            Some("desc") => true,
            Some(_) => return Err(WebError::invalid("dir", "use asc or desc")),
        };
        let kind = match self.kind.as_deref().map(str::trim) {
            None | Some("" | "all") => KindFilter::All,
            Some("folder") => KindFilter::Folder,
            Some("file") => KindFilter::File,
            Some(_) => return Err(WebError::invalid("kind", "use all, folder or file")),
        };
        let hide_recent = match self.hide_recent.as_deref() {
            None => hide_recent_default,
            v => flag(v, "hide_recent")?,
        };
        Ok(ListQuery {
            q: text(self.q.as_deref(), "q", MAX_SEARCH_CHARS)?,
            sort,
            desc,
            hide_recent,
            kind,
            page: bounded(self.page.as_deref(), "page", 1, 1, MAX_PAGE)?,
            per_page: bounded(
                self.per_page.as_deref(),
                "per_page",
                DEFAULT_PER_PAGE,
                1,
                MAX_PER_PAGE,
            )?,
        })
    }
}

// ---- other queries ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct PageQueryRaw {
    pub page: Option<String>,
    pub per_page: Option<String>,
}

impl PageQueryRaw {
    /// `(offset, limit)`.
    pub fn validate(&self) -> V<(u64, u64)> {
        let page = bounded(self.page.as_deref(), "page", 1, 1, MAX_PAGE)?;
        let per = bounded(
            self.per_page.as_deref(),
            "per_page",
            DEFAULT_PER_PAGE,
            1,
            MAX_PER_PAGE,
        )?;
        Ok((u64::from(page - 1) * u64::from(per), u64::from(per)))
    }
}

/// What the statistics chart plots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartMode {
    Cumulative,
    PerDay,
}

impl ChartMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ChartMode::Cumulative => "cumulative",
            ChartMode::PerDay => "per_day",
        }
    }
}

/// Query of `GET /stats`: `view=cumulative|per_day`, `range=all|7d|30d|90d|365d`.
#[derive(Debug, Default, Deserialize)]
pub struct StatsQueryRaw {
    pub view: Option<String>,
    pub range: Option<String>,
}

impl StatsQueryRaw {
    pub fn validate(&self) -> V<(ChartMode, StatsRange)> {
        let mode = match self.view.as_deref().map(str::trim) {
            None | Some("" | "cumulative") => ChartMode::Cumulative,
            Some("per_day") => ChartMode::PerDay,
            Some(_) => return Err(WebError::invalid("view", "use cumulative or per_day")),
        };
        let range = match self.range.as_deref().map(str::trim) {
            None | Some("" | "all") => StatsRange::All,
            Some("7d") => StatsRange::Days7,
            Some("30d") => StatsRange::Days30,
            Some("90d") => StatsRange::Days90,
            Some("365d") => StatsRange::Days365,
            Some(_) => return Err(WebError::invalid("range", "use all, 7d, 30d, 90d or 365d")),
        };
        Ok((mode, range))
    }
}

/// Value of the `range` query parameter of a [`StatsRange`].
#[must_use]
pub fn range_value(r: StatsRange) -> &'static str {
    match r {
        StatsRange::All => "all",
        StatsRange::Days7 => "7d",
        StatsRange::Days30 => "30d",
        StatsRange::Days90 => "90d",
        StatsRange::Days365 => "365d",
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct LogsQueryRaw {
    pub level: Option<String>,
    pub q: Option<String>,
}

impl LogsQueryRaw {
    pub fn validate(&self) -> V<(LogLevel, Option<String>)> {
        let level = match self.level.as_deref().map(str::trim) {
            None | Some("") => LogLevel::Info,
            Some("debug") => LogLevel::Debug,
            Some("info") => LogLevel::Info,
            Some("warn") => LogLevel::Warn,
            Some("error") => LogLevel::Error,
            Some(_) => return Err(WebError::invalid("level", "use debug, info, warn or error")),
        };
        Ok((level, text(self.q.as_deref(), "q", MAX_SEARCH_CHARS)?))
    }
}

// ---- forms -----------------------------------------------------------------------------------

/// Any form carrying a selection of issues (`issue_ids` repeated once per item).
#[derive(Debug, Default, Deserialize)]
pub struct SelectionForm {
    #[serde(default)]
    pub issue_ids: Vec<String>,
}

/// `POST /plans` and `POST /plans/preview`.
#[derive(Debug, Default, Deserialize)]
pub struct PlanForm {
    pub category: Option<String>,
    pub action: Option<String>,
    #[serde(default)]
    pub issue_ids: Vec<String>,
    pub remove_torrent_tasks: Option<String>,
    pub include_specials: Option<String>,
    pub delete_old_media: Option<String>,
}

/// A validated issue plan request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanInput {
    pub category: IssueCategory,
    pub action: IssueAction,
    pub issue_ids: Vec<IssueId>,
    pub remove_torrent_tasks: bool,
    pub include_specials: bool,
}

impl PlanForm {
    pub fn validate(&self) -> V<PlanInput> {
        let category = category(
            self.category
                .as_deref()
                .ok_or_else(|| WebError::invalid("category", "required"))?,
        )?;
        let delete_old = flag(self.delete_old_media.as_deref(), "delete_old_media")?;
        let action = issue_action(
            self.action
                .as_deref()
                .ok_or_else(|| WebError::invalid("action", "required"))?,
            delete_old,
        )?;
        if !action.accepts(category) {
            return Err(WebError::invalid(
                "action",
                "not available for this category",
            ));
        }
        Ok(PlanInput {
            category,
            action,
            issue_ids: issue_ids(&self.issue_ids)?,
            remove_torrent_tasks: flag(
                self.remove_torrent_tasks.as_deref(),
                "remove_torrent_tasks",
            )?,
            include_specials: flag(self.include_specials.as_deref(), "include_specials")?,
        })
    }
}

/// One ARR id (media, episode, indexer): decimal digits, at most 9, never zero for media.
fn arr_id(raw: &str, field: &'static str, allow_zero: bool) -> V<i64> {
    let ok = !raw.is_empty()
        && raw.len() <= 9
        && raw.bytes().all(|b| b.is_ascii_digit())
        && (allow_zero || raw.bytes().any(|b| b != b'0'));
    match raw.parse::<i64>() {
        Ok(v) if ok => Ok(v),
        _ => Err(WebError::invalid(field, "not a valid id")),
    }
}

/// `sonarr` or `radarr` (a path segment).
pub fn arr_service(raw: &str) -> V<ArrService> {
    match raw {
        "sonarr" => Ok(ArrService::Sonarr),
        "radarr" => Ok(ArrService::Radarr),
        _ => Err(WebError::invalid("service", "unknown service")),
    }
}

/// The media of a search or grab request: path ids plus an optional single Sonarr episode.
pub fn arr_media(service: &str, media_id: &str, episode_id: Option<&str>) -> V<ArrMediaRef> {
    let service = arr_service(service)?;
    let media_id = arr_id(media_id, "media_id", false)?;
    let episode_ids = match episode_id.map(str::trim).filter(|s| !s.is_empty()) {
        None => Vec::new(),
        Some(_) if service == ArrService::Radarr => {
            return Err(WebError::invalid("episode_id", "not available for movies"));
        }
        Some(e) => vec![arr_id(e, "episode_id", false)?],
    };
    Ok(ArrMediaRef {
        service,
        media_id,
        episode_ids,
    })
}

/// `POST /media/{service}/{id}/search`.
#[derive(Debug, Default, Deserialize)]
pub struct MediaSearchForm {
    pub episode_id: Option<String>,
}

/// `POST /media/{service}/{id}/grab-plan`: the opaque token of a release of a fresh search.
#[derive(Debug, Default, Deserialize)]
pub struct GrabPlanForm {
    pub episode_id: Option<String>,
    pub release: Option<String>,
    pub indexer_id: Option<String>,
}

impl GrabPlanForm {
    pub fn validate(&self) -> V<ReleaseRef> {
        let token = self
            .release
            .as_deref()
            .ok_or_else(|| WebError::invalid("release", "required"))?;
        let body = token.strip_prefix("rel-").unwrap_or("");
        if !hex_of_len(body, 32) {
            return Err(WebError::invalid("release", "not a valid release token"));
        }
        let indexer_id = arr_id(
            self.indexer_id
                .as_deref()
                .ok_or_else(|| WebError::invalid("indexer_id", "required"))?,
            "indexer_id",
            true,
        )?;
        Ok(ReleaseRef {
            guid: token.to_string(),
            indexer_id,
        })
    }
}

/// `POST /plans/{id}/approve`.
#[derive(Debug, Default, Deserialize)]
pub struct ApproveForm {
    pub plan_hash: Option<String>,
    pub confirmed_count: Option<String>,
    pub confirmed_torrent_removals: Option<String>,
    #[serde(default)]
    pub acknowledged_samples: Vec<String>,
    #[serde(default)]
    pub acknowledged_unrecoverable: Vec<String>,
}

impl ApproveForm {
    pub fn validate(&self) -> V<removarr_domain::plan::ApprovalRequest> {
        Ok(removarr_domain::plan::ApprovalRequest {
            plan_hash: plan_hash(
                self.plan_hash
                    .as_deref()
                    .ok_or_else(|| WebError::invalid("plan_hash", "required"))?,
            )?,
            confirmed_count: count(self.confirmed_count.as_deref(), "confirmed_count")?,
            confirmed_torrent_removals: count(
                self.confirmed_torrent_removals.as_deref(),
                "confirmed_torrent_removals",
            )?,
            acknowledged_samples: rooted_paths(&self.acknowledged_samples, "acknowledged_samples")?,
            acknowledged_unrecoverable: rooted_paths(
                &self.acknowledged_unrecoverable,
                "acknowledged_unrecoverable",
            )?,
        })
    }
}

/// `POST /issues/ignore` and `/issues/restore`: explicit `subjects`, or the `issue_ids` of the
/// selection (resolved to their subjects), plus an optional reason.
#[derive(Debug, Default, Deserialize)]
pub struct IgnoreForm {
    #[serde(default)]
    pub subjects: Vec<String>,
    #[serde(default)]
    pub issue_ids: Vec<String>,
    pub reason: Option<String>,
}

/// `POST /locale`.
#[derive(Debug, Default, Deserialize)]
pub struct LocaleForm {
    pub locale: Option<String>,
    /// Page the user was on (same-origin relative path); anything else is ignored.
    pub return_to: Option<String>,
}

/// Longest accepted return path (path and query).
pub const MAX_RETURN_BYTES: usize = 512;

impl LocaleForm {
    /// Where to go after the change: the submitted path when it is a same-origin relative path
    /// (one leading `/`, no scheme, no host, no control character, no backslash), else `/`.
    #[must_use]
    pub fn return_path(&self) -> String {
        let Some(p) = self.return_to.as_deref() else {
            return "/".to_string();
        };
        let ok = p.len() <= MAX_RETURN_BYTES
            && p.starts_with('/')
            && !p.starts_with("//")
            && !p.contains('\\')
            && !p
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || !c.is_ascii());
        if ok { p.to_string() } else { "/".to_string() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    const A: &str = "0123456789abcdef0123456789abcdef";
    const B: &str = "fedcba9876543210fedcba9876543210";

    #[test]
    fn selections_are_explicit_valid_and_bounded() {
        assert!(issue_ids(&[]).is_err());
        assert_eq!(issue_ids(&ids(&[B, A, A])).unwrap().len(), 2);
        for bad in [
            "",
            "0123",
            "0123456789ABCDEF0123456789ABCDEF",
            "../../etc/passwd",
            "0123456789abcdef0123456789abcdeg",
        ] {
            assert!(issue_ids(&ids(&[A, bad])).is_err(), "{bad:?}");
        }
        let many = vec![A.to_string(); MAX_SELECTION + 1];
        assert!(issue_ids(&many).is_err());
    }

    #[test]
    fn list_query_defaults_and_rejections() {
        let q = ListQueryRaw::default().validate(true).unwrap();
        assert_eq!(
            (q.sort, q.desc, q.page, q.per_page, q.hide_recent),
            (SortKey::Size, true, 1, DEFAULT_PER_PAGE, true)
        );
        let bad = |f: fn(&mut ListQueryRaw)| {
            let mut r = ListQueryRaw::default();
            f(&mut r);
            r.validate(true).is_err()
        };
        assert!(bad(|r| r.sort = Some("inode".into())));
        assert!(bad(|r| r.dir = Some("up".into())));
        assert!(bad(|r| r.kind = Some("pair".into())));
        assert!(bad(|r| r.page = Some("0".into())));
        assert!(bad(|r| r.page = Some("-1".into())));
        assert!(bad(|r| r.per_page = Some("201".into())));
        assert!(bad(|r| r.per_page = Some("1e3".into())));
        assert!(bad(|r| r.hide_recent = Some("maybe".into())));
        assert!(bad(|r| r.q = Some("x".repeat(MAX_SEARCH_CHARS + 1))));
        assert!(bad(|r| r.q = Some("a\u{0}b".into())));
    }

    #[test]
    fn plan_form_rejects_mismatched_actions_and_ids() {
        let mut f = PlanForm {
            category: Some("junk".into()),
            action: Some("consolidate".into()),
            issue_ids: ids(&[A]),
            ..PlanForm::default()
        };
        assert!(f.validate().is_err());
        f.action = Some("delete".into());
        let ok = f.validate().unwrap();
        assert_eq!(ok.action, IssueAction::Delete);
        f.category = Some("Junk".into());
        assert!(f.validate().is_err());
    }

    #[test]
    fn approval_form_needs_a_hash_and_numeric_counts() {
        let hash = "a".repeat(64);
        let mut f = ApproveForm {
            plan_hash: Some(hash.clone()),
            confirmed_count: Some("3".into()),
            acknowledged_samples: ids(&["completed:Rel/Sample/s.mkv"]),
            ..ApproveForm::default()
        };
        let r = f.validate().unwrap();
        assert_eq!(r.confirmed_count, 3);
        assert_eq!(r.acknowledged_samples.len(), 1);
        f.confirmed_count = Some("three".into());
        assert!(f.validate().is_err());
        f.confirmed_count = None;
        f.acknowledged_samples = ids(&["/abs/path"]);
        assert!(f.validate().is_err());
        f.acknowledged_samples = vec![];
        f.plan_hash = Some("short".into());
        assert!(f.validate().is_err());
    }

    #[test]
    fn locale_return_path_is_a_same_origin_relative_path() {
        let ret = |v: &str| {
            LocaleForm {
                locale: None,
                return_to: Some(v.to_string()),
            }
            .return_path()
        };
        assert_eq!(
            ret("/issues/junk?page=2&q=a%20b"),
            "/issues/junk?page=2&q=a%20b"
        );
        assert_eq!(ret("/"), "/");
        for bad in [
            "//evil.example/x",
            "https://evil.example/",
            "http:evil.example",
            "/\\evil.example",
            "\\\\evil.example",
            "evil.example",
            "",
            "/a\nSet-Cookie: x=y",
            "/a b",
            "/é",
        ] {
            assert_eq!(ret(bad), "/", "{bad:?}");
        }
        assert_eq!(ret(&format!("/{}", "a".repeat(MAX_RETURN_BYTES))), "/");
        assert_eq!(LocaleForm::default().return_path(), "/");
    }

    #[test]
    fn subjects_and_reasons_are_bounded() {
        assert!(subjects(&[]).is_err());
        assert!(subjects(&ids(&["nocolon"])).is_err());
        assert!(subjects(&ids(&["p:completed:a\nb"])).is_err());
        assert_eq!(subjects(&ids(&["t:abc", "t:abc"])).unwrap().len(), 1);
        assert!(
            text(
                Some(&"r".repeat(MAX_REASON_CHARS + 1)),
                "reason",
                MAX_REASON_CHARS
            )
            .is_err()
        );
        assert_eq!(text(Some("  "), "reason", 10).unwrap(), None);
    }
}
