//! Minimal `GET /login` page. The sign-in flow itself (`POST /login`, `POST /logout`, sessions,
//! CSRF, rate limit) belongs to `auth-guard` (17), which replaces this handler's logic; until
//! then the page only says that sign-in is unavailable and every protected route is refused.

use askama::Template;
use axum::response::Response;

use super::{Chrome, Ctx, render};
use crate::i18n::Tr;

/// `LoginVm { error_key, retry_after }` (architecture 12.2).
#[derive(Debug, Clone, Default)]
pub struct LoginView {
    pub error_key: Option<&'static str>,
    pub retry_after: Option<u64>,
}

#[derive(Debug, Template)]
#[template(path = "pages/login.html")]
pub struct LoginPage<'a> {
    pub ctx: &'a Ctx,
    pub v: &'a LoginView,
}

/// `GET /login` (public).
pub async fn page(tr: Tr) -> Response {
    let ctx = Ctx {
        tr,
        htmx: false,
        chrome: Chrome {
            csrf: String::new(),
            user: String::new(),
            dry_run: false,
            lang: tr.locale.as_str(),
        },
    };
    let v = LoginView {
        error_key: Some("login.unavailable"),
        retry_after: None,
    };
    let page = LoginPage { ctx: &ctx, v: &v };
    render(&ctx, &page, &page)
}
