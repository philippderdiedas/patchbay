// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared page context, static assets, language switch and target selector data.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, header},
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use sqlx::PgPool;

use crate::{
    AppError, AppState,
    auth::{CurrentUser, NavStatus},
    i18n::{LANG_COOKIE, Lang, Tr},
};

const BASE_CSS: &str = include_str!("../static/base.css");
const APP_JS: &str = include_str!("../static/app.js");
/// project logo (docs/logo.svg) and icons rendered from it (32 px favicon, 180 px home screen icon)
const LOGO_SVG: &str = include_str!("../static/logo.svg");
const ICON_32: &[u8] = include_bytes!("../static/icon-32.png");
const ICON_180: &[u8] = include_bytes!("../static/icon-180.png");

/// Site title for pages rendered without access to the app state (errors).
pub static SITE_TITLE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// A page template as the HTML response (all page handlers).
pub fn render(t: impl askama::Template) -> Result<Response, AppError> {
    Ok(Html(t.render().map_err(anyhow::Error::from)?).into_response())
}

/// Context every page template receives as `c`.
pub struct Ctx {
    /// running version (footer for admins)
    pub version: &'static str,
    pub name: String,
    pub is_admin: bool,
    pub t: Tr,
    pub lang: &'static str,
    pub site_title: String,
    /// current admin section for the sub navigation
    pub section: &'static str,
    /// active main navigation entry ("call", "calls", "voicemail", "phonebook", "help", "admin" or "")
    pub nav: &'static str,
    /// DND / forward / unheard voicemails for the header (None: not loaded, e.g. no phone account)
    pub status: Option<NavStatus>,
}

/// Main navigation entries: (key, path prefixes). The longest matching prefix wins.
const NAV: &[(&str, &[&str])] = &[
    // start page with the person's own settings (devices, DND, forwarding, groups, apps)
    ("home", &["/", "/me", "/devices", "/apps", "/media"]),
    ("calls", &["/calls"]),
    ("voicemail", &["/voicemail"]),
    ("phonebook", &["/phonebook", "/contacts"]),
    ("help", &["/help"]),
    ("admin", &["/admin"]),
];

/// Active main navigation entry for a request path: prefix match on whole path segments
/// ("/calls" does not match "/call"), longest prefix wins.
pub fn nav_for(path: &str) -> &'static str {
    let mut best: (&'static str, usize) = ("", 0);
    for (key, prefixes) in NAV {
        for p in *prefixes {
            let hit = path == *p || path.strip_prefix(p).is_some_and(|rest| rest.starts_with('/') || rest.starts_with('.'));
            if hit && p.len() > best.1 {
                best = (key, p.len());
            }
        }
    }
    best.0
}

impl Ctx {
    /// The person has a phone account: show the call companion widget.
    pub fn has_phone(&self) -> bool {
        self.status.is_some()
    }

    /// Target descriptions built in SQL ([`target_desc!`]: "Call group 5120 · Sales"): the leading
    /// kind word in the UI language.
    pub fn tdesc(&self, s: &str) -> String {
        for k in TDESC_KINDS {
            if let Some(rest) = s.strip_prefix(k) {
                if rest.is_empty() || rest.starts_with(' ') {
                    return format!("{}{rest}", self.t.s(k));
                }
            }
        }
        s.to_string()
    }

    /// App kind ("conference", "custom" …) as a label in the UI language (`apps::kind_label`).
    pub fn app_kind(&self, kind: &str) -> &'static str {
        self.t.s(crate::apps::kind_label(kind))
    }

    /// Translations for the companion widget (JSON, read by app.js).
    pub fn companion_i18n(&self) -> String {
        crate::companion::i18n(&self.t)
    }

    pub fn new(app: &AppState, u: &CurrentUser) -> Self {
        Ctx {
            name: u.session.name.clone(),
            is_admin: u.is_admin,
            t: Tr(u.lang),
            lang: u.lang.code(),
            version: crate::config::build_version(),
            site_title: app.cfg.site_title.clone(),
            section: "",
            nav: nav_for(&u.path),
            status: u.nav.clone(),
        }
    }

    /// Context for pages without a session (logout page, errors).
    pub fn anonymous(lang: Lang) -> Self {
        Ctx {
            name: String::new(),
            is_admin: false,
            t: Tr(lang),
            lang: lang.code(),
            version: crate::config::build_version(),
            site_title: SITE_TITLE.get().cloned().unwrap_or_else(|| "Phone system".into()),
            section: "",
            nav: "",
            status: None,
        }
    }

    pub fn section(mut self, s: &'static str) -> Self {
        self.section = s;
        self
    }

    /// For templates: is `key` the active main navigation entry
    pub fn is_nav(&self, key: &str) -> bool {
        self.nav == key
    }

    /// Unheard voicemails for the badge (0 = no badge)
    pub fn unheard(&self) -> i64 {
        self.status.as_ref().map_or(0, |s| s.unheard)
    }

    /// Header chip: do not disturb is on
    pub fn dnd_on(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.dnd)
    }

    /// Header chip: immediate forward target, empty = no forward
    pub fn forward_to(&self) -> &str {
        self.status.as_ref().and_then(|s| s.forward.as_deref()).unwrap_or("")
    }
}

fn asset(content_type: &'static str, body: String) -> Response {
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

pub async fn base_css() -> Response {
    asset("text/css; charset=utf-8", BASE_CSS.to_string())
}

pub async fn app_js() -> Response {
    asset("text/javascript; charset=utf-8", APP_JS.to_string())
}

/// Site files that replace the project logo and icons: `/config/<name>` (= $PBX_DIR/config),
/// read on every request (small files), so a site needs no restart and no variable.
const SITE_DIR: &str = "/config";

/// Serves a site file if present, else the embedded default. Site files come with a sandbox CSP:
/// an SVG opened directly as a document cannot run script or reach the session.
async fn site_or_default(names: &[(&str, &'static str)], default: (&'static str, &'static [u8])) -> Response {
    for (name, ctype) in names {
        if let Ok(body) = tokio::fs::read(format!("{SITE_DIR}/{name}")).await {
            return (
                [
                    (header::CONTENT_TYPE, *ctype),
                    (header::CACHE_CONTROL, "no-cache"),
                    (header::CONTENT_SECURITY_POLICY, "default-src 'none'; style-src 'unsafe-inline'; img-src data:; sandbox"),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                ],
                body,
            )
                .into_response();
        }
    }
    ([(header::CONTENT_TYPE, default.0), (header::CACHE_CONTROL, "no-cache")], default.1).into_response()
}

/// Logo (navigation, login page, favicon): site `logo.svg` or `logo.png`, else Patchbay's.
pub async fn logo() -> Response {
    site_or_default(&[("logo.svg", "image/svg+xml"), ("logo.png", "image/png")], ("image/svg+xml", LOGO_SVG.as_bytes())).await
}

/// 32 px favicon (older browsers, /favicon.ico): site `icon-32.png`, else Patchbay's.
pub async fn icon_32() -> Response {
    site_or_default(&[("icon-32.png", "image/png")], ("image/png", ICON_32)).await
}

/// 180 px home screen icon (iOS): site `icon-180.png`, else Patchbay's.
pub async fn icon_180() -> Response {
    site_or_default(&[("icon-180.png", "image/png")], ("image/png", ICON_180)).await
}

/// Site theme (PBX_THEME_CSS), empty if the site has none.
pub async fn theme_css(State(app): State<AppState>) -> Response {
    let css = match &app.cfg.theme_css {
        Some(p) => tokio::fs::read_to_string(p).await.unwrap_or_default(),
        None => String::new(),
    };
    asset("text/css; charset=utf-8", css)
}

pub async fn set_lang(
    State(app): State<AppState>,
    jar: CookieJar,
    private: axum_extra::extract::cookie::PrivateCookieJar,
    headers: HeaderMap,
    Path(lang): Path<String>,
) -> Result<Response, AppError> {
    let lang = Lang::parse(&lang).ok_or(AppError::NotFound)?;
    // Logged-in users: remember the choice (also used for notification mails)
    if let Some(uid) = crate::auth::session_from(&private).and_then(|s| s.user_id) {
        sqlx::query("UPDATE users SET ui_language = $2 WHERE id = $1")
            .bind(uid)
            .bind(lang.code())
            .execute(&app.db)
            .await
            .map_err(anyhow::Error::from)?;
    }
    let cookie = Cookie::build((LANG_COOKIE, lang.code()))
        .path("/")
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::days(365))
        .build();
    // Only redirect back to our own pages
    let back = headers
        .get(header::REFERER)
        .and_then(|v| v.to_str().ok())
        .and_then(|r| r.find("://").and_then(|i| r[i + 3..].find('/').map(|j| r[i + 3 + j..].to_string())))
        .filter(|p| p.starts_with('/') && !p.starts_with("//"))
        .unwrap_or_else(|| "/".into());
    Ok((jar.add(cookie), Redirect::to(&back)).into_response())
}

// --- Targets in SQL ----------------------------------------------------------------------------
// Macros (string literals) so that constant queries can include them with concat!.

/// Joins for a target row `t`: tu = person, tg = call group, ti = IVR, ts = announcement, ta = app.
macro_rules! target_joins {
    () => {
        "LEFT JOIN users tu ON tu.id = t.user_id LEFT JOIN groups tg ON tg.id = t.group_id
         LEFT JOIN ivrs ti ON ti.id = t.ivr_id LEFT JOIN sounds ts ON ts.id = t.sound_id
         LEFT JOIN apps ta ON ta.id = t.app_id"
    };
}
pub(crate) use target_joins;

/// Value of a target row `t` as the target selector expects it (extension, group number, ids,
/// external number); needs tu and tg of [`target_joins!`].
macro_rules! target_value {
    () => {
        "COALESCE(tu.extension::text, tg.number::text, t.ivr_id::text, t.sound_id::text, t.external_e164, ta.number::text)"
    };
}
pub(crate) use target_value;

/// English description of a target row `t` ("Call group 5120 · Sales"), translated by
/// `Ctx::tdesc` (leading words: [`TDESC_KINDS`]); needs [`target_joins!`].
macro_rules! target_desc {
    () => {
        "CASE t.kind WHEN 'user' THEN 'Person ' || tu.extension || ' · ' || tu.display_name
        WHEN 'mailbox_user' THEN 'Voicemail ' || tu.extension || ' · ' || tu.display_name
        WHEN 'mailbox_group' THEN 'Voicemail ' || tg.number || ' · ' || tg.name
        WHEN 'group' THEN 'Call group ' || tg.number || ' · ' || tg.name
        WHEN 'ivr' THEN 'IVR ' || ti.name WHEN 'announcement' THEN 'Announcement ' || ts.name
        WHEN 'external' THEN 'External ' || t.external_e164 WHEN 'hangup' THEN 'Hang up'
        WHEN 'app' THEN 'App ' || ta.number || ' · ' || ta.name END"
    };
}
pub(crate) use target_desc;

pub const TARGET_DESC: &str = target_desc!();
pub const TARGET_JOINS: &str = target_joins!();

/// Leading kind words of [`target_desc!`], translated by `Ctx::tdesc`.
const TDESC_KINDS: [&str; 8] = ["Call group", "Voicemail", "Announcement", "External", "Person", "IVR", "Hang up", "App"];

/// Options for target selectors (person, group, IVR, announcement, app).
pub struct TargetOptions {
    pub users: Vec<(String, String)>,
    pub groups: Vec<(String, String)>,
    pub ivrs: Vec<(String, String)>,
    pub sounds: Vec<(String, String)>,
    pub apps: Vec<(String, String)>,
}

impl TargetOptions {
    pub async fn load(db: &PgPool) -> Result<Self, AppError> {
        let q = |sql: &'static str| async move {
            sqlx::query_as::<_, (String, String)>(sql).fetch_all(db).await.map_err(anyhow::Error::from)
        };
        Ok(TargetOptions {
            users: q("SELECT extension::text, extension || ' · ' || display_name FROM users WHERE deleted_at IS NULL ORDER BY extension").await?,
            groups: q("SELECT number::text, number || ' · ' || name FROM groups WHERE deleted_at IS NULL ORDER BY number").await?,
            ivrs: q("SELECT id::text, name FROM ivrs WHERE deleted_at IS NULL ORDER BY name").await?,
            sounds: q("SELECT id::text, name FROM sounds WHERE deleted_at IS NULL AND owner_user_id IS NULL ORDER BY name").await?,
            // organization apps only (admin::save_target)
            apps: q("SELECT number::text, number || ' · ' || name FROM apps WHERE deleted_at IS NULL AND owner_user_id IS NULL ORDER BY number").await?,
        })
    }
}

/// One target selector in a form: field prefix, current kind/value, label for "no target",
/// and which kinds are offered.
#[derive(Clone)]
pub struct TargetSel {
    pub prefix: &'static str,
    pub kind: String,
    pub value: String,
    pub none_label: &'static str,
    pub kinds: &'static [&'static str],
}

pub const ALL_KINDS: &[&str] = &["user", "group", "mailbox_user", "mailbox_group", "ivr", "app", "announcement", "external", "hangup"];

impl TargetSel {
    pub fn new(prefix: &'static str, kind: Option<String>, value: Option<String>, none_label: &'static str) -> Self {
        TargetSel {
            prefix,
            kind: kind.unwrap_or_default(),
            value: value.unwrap_or_default(),
            none_label,
            kinds: ALL_KINDS,
        }
    }

    pub fn offers(&self, k: &str) -> bool {
        self.kinds.contains(&k)
    }

    pub fn is(&self, k: &str) -> bool {
        self.kind == k
    }

    pub fn value_is(&self, k: &str, v: &str) -> bool {
        self.kind == k && self.value == v
    }
}

#[cfg(test)]
mod tests {
    use super::nav_for;

    /// Every kind word the SQL writes is one that Ctx::tdesc translates.
    #[test]
    fn target_desc_kinds_are_translated() {
        let labels: Vec<&str> = super::TARGET_DESC.split("THEN '").skip(1).map(|r| r[..r.find('\'').unwrap()].trim_end()).collect();
        assert_eq!(labels.len(), 9);
        for l in labels {
            assert!(super::TDESC_KINDS.contains(&l), "{l}");
        }
    }

    #[test]
    fn active_nav_entry() {
        assert_eq!(nav_for("/call"), "");
        assert_eq!(nav_for("/"), "home");
        assert_eq!(nav_for("/apps/3"), "home");
        assert_eq!(nav_for("/admin/apps"), "admin");
        assert_eq!(nav_for("/calls"), "calls");
        assert_eq!(nav_for("/calls.csv"), "calls");
        assert_eq!(nav_for("/callsx"), "");
        assert_eq!(nav_for("/voicemail"), "voicemail");
        assert_eq!(nav_for("/contacts/12"), "phonebook");
        assert_eq!(nav_for("/admin/groups/3"), "admin");
        assert_eq!(nav_for("/me/chat"), "home");
    }
}
