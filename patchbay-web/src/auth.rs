// SPDX-License-Identifier: GPL-3.0-or-later
//! Login via OIDC (Authorization Code + PKCE, usually Keycloak) or local accounts (see local.rs),
//! session in an encrypted cookie.
//!
//! Permissions (docs/configuration.md, "Login"):
//! * local login (`sub` = `local:<user id>`): the per-user switches `users.web_access` /
//!   `users.web_admin`; phone features follow `users.phone_allowed`.
//! * OIDC login: the groups PBX_GROUP_WEBUI / PBX_GROUP_ADMIN from the token (linked users: from
//!   the database, refreshed on login and by the Keycloak sync) OR the same per-user switches.
//! * `automation:*` sessions (mint-session) keep their minted groups.

use anyhow::{Context, Result, anyhow};
use axum::{
    extract::{FromRef, FromRequestParts, Query, State},
    http::request::Parts,
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::{Cookie, Key, PrivateCookieJar, SameSite};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet, EndpointNotSet,
    EndpointSet, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
    OAuth2TokenResponse, TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
};
use serde::{Deserialize, Serialize};

use crate::{AppError, AppState, db};

const SESSION_COOKIE: &str = "pbx_session";
const LOGIN_COOKIE: &str = "pbx_login";
const SESSION_HOURS: i64 = 10;

pub type OidcClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

pub struct Oidc {
    pub client: OidcClient,
    pub http: reqwest::Client,
    pub scopes: Vec<String>,
}

impl Oidc {
    pub async fn discover(cfg: &crate::config::OidcConfig, public_url: &str) -> Result<Self> {
        let http = reqwest::ClientBuilder::new()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let meta = CoreProviderMetadata::discover_async(IssuerUrl::new(cfg.issuer.clone())?, &http)
            .await
            .context("OIDC discovery failed")?;
        let client = CoreClient::from_provider_metadata(
            meta,
            ClientId::new(cfg.client_id.clone()),
            Some(ClientSecret::new(cfg.client_secret.clone())),
        )
        .set_redirect_uri(RedirectUrl::new(format!("{public_url}/auth/callback"))?);
        Ok(Self { client, http, scopes: cfg.scopes.clone() })
    }
}

/// The OIDC client, discovered on first use (with PBX_AUTH=oidc already at startup). With
/// PBX_AUTH=both a failed discovery is retried on the next OIDC login, local logins keep working.
pub async fn oidc(app: &AppState) -> Result<&Oidc> {
    let cfg = app.cfg.oidc.as_ref().ok_or_else(|| anyhow!("OIDC login is not enabled (PBX_AUTH)"))?;
    app.oidc.get_or_try_init(|| Oidc::discover(cfg, &app.cfg.public_url)).await
}

/// Prefix of the session subject of local logins (`local:<users.id>`).
pub const LOCAL_SUB: &str = "local:";

/// Contents of the session cookie.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Session {
    pub sub: String,
    pub name: String,
    pub email: Option<String>,
    pub groups: Vec<String>,
    /// users.id if the Keycloak account is linked to a person in the PBX
    pub user_id: Option<i32>,
    pub exp: i64,
    /// local logins: password version (ms since epoch of the last password change); a password
    /// change or reset ends all other sessions of that login
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pwv: Option<i64>,
}

impl Session {
    pub fn is_local(&self) -> bool {
        self.sub.starts_with(LOCAL_SUB)
    }
}

#[derive(Serialize, Deserialize)]
struct LoginState {
    csrf: String,
    nonce: String,
    pkce: String,
}

/// Logged-in user with web UI permission.
pub struct CurrentUser {
    pub session: Session,
    pub is_admin: bool,
    /// may use the phone features (own devices, apps): users.phone_allowed of the linked person
    pub may_phone: bool,
    pub lang: crate::i18n::Lang,
    /// request path, for the active navigation entry
    pub path: String,
    /// DND / forward / unheard voicemails for the header (HTML page requests of users with a
    /// phone account only)
    pub nav: Option<NavStatus>,
}

impl CurrentUser {
    /// The linked person (users.id); pages that need a phone account answer 403 without one.
    pub fn phone_uid(&self) -> Result<i32, AppError> {
        self.session.user_id.ok_or_else(|| AppError::Forbidden("No phone account.".into()))
    }
}

/// Call diversion and mailbox state shown in the header on every page.
#[derive(Clone, Debug, Default)]
pub struct NavStatus {
    pub dnd: bool,
    /// immediate forward target ("1021", "+49176…"), None = no forward
    pub forward: Option<String>,
    /// unheard messages in the mailboxes the user may access (own + call groups)
    pub unheard: i64,
}

impl NavStatus {
    /// One query; None on errors (the header just shows nothing then).
    pub async fn load(db: &sqlx::PgPool, uid: i32) -> Option<NavStatus> {
        let row: Option<(bool, Option<String>, i64)> = sqlx::query_as(&format!(
            "SELECT u.dnd,
                    (SELECT COALESCE(tu.extension::text, tg.number::text, ti.number::text, t.external_e164, '?')
                     FROM targets t LEFT JOIN users tu ON tu.id = t.user_id LEFT JOIN groups tg ON tg.id = t.group_id
                     LEFT JOIN ivrs ti ON ti.id = t.ivr_id WHERE t.id = u.forward_immediate),
                    (SELECT count(*) FROM voicemails v WHERE v.heard_at IS NULL AND {})
             FROM users u WHERE u.id = $1",
            crate::voicemail::ACCESS
        ))
        .bind(uid)
        .fetch_optional(db)
        .await
        .map_err(|e| tracing::warn!("header status: {e}"))
        .ok()
        .flatten();
        row.map(|(dnd, forward, unheard)| NavStatus { dnd, forward, unheard })
    }
}

/// Header data only for page loads (not for POST forms, JSON polling or audio).
fn is_page_request(parts: &Parts) -> bool {
    parts.method == axum::http::Method::GET
        && parts
            .headers
            .get(axum::http::header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|a| a.contains("text/html"))
}

/// Like CurrentUser, but only for members of the admin group.
pub struct Admin(pub CurrentUser);

impl FromRef<AppState> for Key {
    fn from_ref(state: &AppState) -> Self {
        state.cookie_key.clone()
    }
}

impl<S> FromRequestParts<S> for CurrentUser
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app = AppState::from_ref(state);
        let jar = PrivateCookieJar::<Key>::from_headers(&parts.headers, app.cookie_key.clone());
        let session = jar
            .get(SESSION_COOKIE)
            .and_then(|c| serde_json::from_str::<Session>(c.value()).ok())
            .filter(|s| s.exp > chrono::Utc::now().timestamp());
        let Some(session) = session else {
            return Err(Redirect::to("/auth/login").into_response());
        };
        let cfg = &app.cfg;
        let mut session = session;
        let inactive = || AppError::Forbidden("Your phone account is no longer active.".into()).into_response();
        let db_err = |e: sqlx::Error| AppError::Internal(e.into()).into_response();
        let mut switches = (false, false); // (web_access, web_admin)
        let may_phone;
        if session.sub.starts_with("automation:") {
            // automation sessions (mint-session) keep their minted groups
            if let Some(uid) = session.user_id {
                let active: Option<bool> = sqlx::query_scalar("SELECT enabled AND deleted_at IS NULL FROM users WHERE id = $1")
                    .bind(uid)
                    .fetch_optional(&app.db)
                    .await
                    .map_err(db_err)?;
                if active != Some(true) {
                    return Err(inactive());
                }
            }
            may_phone = session.groups.contains(&cfg.group_phone);
        } else if session.is_local() {
            // Local logins: the login must still exist with the same password version and the
            // local login method must still be enabled.
            let Some(uid) = session.user_id.filter(|_| cfg.auth.local()) else {
                return Err(Redirect::to("/auth/login").into_response());
            };
            let row: Option<(bool, bool, bool, bool, i64)> = sqlx::query_as(
                "SELECT u.enabled AND u.deleted_at IS NULL, u.web_access, u.web_admin, u.phone_allowed,
                        (extract(epoch FROM l.password_changed_at) * 1000)::bigint
                 FROM users u JOIN local_logins l ON l.user_id = u.id WHERE u.id = $1",
            )
            .bind(uid)
            .fetch_optional(&app.db)
            .await
            .map_err(db_err)?;
            match row {
                Some((true, web, admin, phone, pwv)) if session.pwv == Some(pwv) && session.sub == format!("{LOCAL_SUB}{uid}") => {
                    switches = (web, admin);
                    may_phone = phone;
                    session.groups.clear();
                }
                // password changed elsewhere: log in again
                Some((true, ..)) => return Err(Redirect::to("/auth/login").into_response()),
                _ => return Err(inactive()),
            }
        } else if !cfg.auth.oidc() {
            return Err(Redirect::to("/auth/login").into_response());
        } else if let Some(uid) = session.user_id {
            // Linked OIDC users: groups come from the database (login + Keycloak sync every 10 min),
            // so a removed group or a disabled/deleted user takes effect without waiting for the
            // cookie to expire.
            let row: Option<(Vec<String>, bool, bool, bool, bool)> = sqlx::query_as(
                "SELECT kc_groups, enabled AND deleted_at IS NULL, web_access, web_admin, phone_allowed
                 FROM users WHERE id = $1 AND kc_subject = $2",
            )
            .bind(uid)
            .bind(&session.sub)
            .fetch_optional(&app.db)
            .await
            .map_err(db_err)?;
            match row {
                Some((groups, true, web, admin, phone)) => {
                    // Keycloak sync with a provisioning mode that uses the phone group: membership is
                    // required as well (phone_allowed follows it); otherwise the admin's switch alone
                    may_phone = phone && (!cfg.kc_sync || !cfg.provision.needs_phone_group() || groups.contains(&cfg.group_phone));
                    session.groups = groups;
                    switches = (web, admin);
                }
                _ => return Err(inactive()),
            }
        } else {
            // OIDC account without a phone account
            may_phone = false;
        }
        let (is_admin, web) = permissions(cfg, &session.groups, switches.0, switches.1);
        if !web {
            return Err(AppError::Forbidden("No permission for the phone system.".into()).into_response());
        }
        let lang = crate::i18n::Lang::from_headers(&parts.headers);
        let nav = match session.user_id {
            Some(uid) if is_page_request(parts) => NavStatus::load(&app.db, uid).await,
            _ => None,
        };
        Ok(CurrentUser { session, is_admin, may_phone, lang, path: parts.uri.path().to_string(), nav })
    }
}

impl<S> FromRequestParts<S> for Admin
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let user = CurrentUser::from_request_parts(parts, state).await?;
        if !user.is_admin {
            return Err(AppError::Forbidden("Admins only.".into()).into_response());
        }
        Ok(Admin(user))
    }
}

/// (is_admin, may use the web UI) from OIDC groups and the per-user switches.
pub fn permissions(cfg: &crate::config::Config, groups: &[String], web_access: bool, web_admin: bool) -> (bool, bool) {
    let admin = web_admin || groups.contains(&cfg.group_admin);
    (admin, admin || web_access || groups.contains(&cfg.group_webui))
}

/// Sets the session cookie and the UI language cookie.
pub fn start_session(jar: PrivateCookieJar, session: &Session, lang: crate::i18n::Lang) -> Result<PrivateCookieJar> {
    Ok(jar
        .add(cookie(SESSION_COOKIE, serde_json::to_string(session)?, SESSION_HOURS * 60))
        .add(
            Cookie::build((crate::i18n::LANG_COOKIE, lang.code()))
                .path("/")
                .secure(true)
                .same_site(SameSite::Lax)
                .max_age(time::Duration::days(365))
                .build(),
        ))
}

/// Expiry of a new session.
pub fn session_exp() -> i64 {
    chrono::Utc::now().timestamp() + SESSION_HOURS * 3600
}

fn cookie(name: &'static str, value: String, minutes: i64) -> Cookie<'static> {
    Cookie::build((name, value))
        .path("/")
        .secure(true)
        .http_only(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::minutes(minutes))
        .build()
}

/// Reads the session from the cookie jar (None if missing or expired).
pub fn session_from(jar: &PrivateCookieJar) -> Option<Session> {
    jar.get(SESSION_COOKIE)
        .and_then(|c| serde_json::from_str::<Session>(c.value()).ok())
        .filter(|s| s.exp > chrono::Utc::now().timestamp())
}

/// GET /auth/login: straight to the identity provider with PBX_AUTH=oidc, otherwise the login form.
pub async fn login(State(app): State<AppState>, jar: PrivateCookieJar, headers: axum::http::HeaderMap) -> Result<Response, AppError> {
    if app.cfg.auth.local() {
        return crate::local::login_page(&app, &headers, None);
    }
    oidc_start(State(app), jar).await
}

/// GET /auth/oidc: starts the OIDC login.
pub async fn oidc_start(State(app): State<AppState>, jar: PrivateCookieJar) -> Result<Response, AppError> {
    if !app.cfg.auth.oidc() {
        return Err(AppError::NotFound);
    }
    let oidc = oidc(&app).await?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let mut req = oidc.client.authorize_url(
        CoreAuthenticationFlow::AuthorizationCode,
        CsrfToken::new_random,
        Nonce::new_random,
    );
    for s in &oidc.scopes {
        req = req.add_scope(Scope::new(s.clone()));
    }
    let (url, csrf, nonce) = req.set_pkce_challenge(challenge).url();
    let state = LoginState {
        csrf: csrf.secret().clone(),
        nonce: nonce.secret().clone(),
        pkce: verifier.secret().clone(),
    };
    let jar = jar.add(cookie(LOGIN_COOKIE, serde_json::to_string(&state)?, 10));
    Ok((jar, Redirect::to(url.as_str())).into_response())
}

#[derive(Deserialize)]
pub struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Reads a claim from the payload of an already verified JWT.
fn jwt_claim(jwt: &str, name: &str) -> Option<serde_json::Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get(name).cloned()
}

fn groups_from(v: Option<serde_json::Value>) -> Vec<String> {
    v.and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
        .unwrap_or_default()
        .into_iter()
        // Depending on the mapper, Keycloak returns "/group" or "group"
        .map(|g| g.trim_start_matches('/').to_string())
        .collect()
}

pub async fn callback(
    State(app): State<AppState>,
    jar: PrivateCookieJar,
    headers: axum::http::HeaderMap,
    Query(p): Query<CallbackParams>,
) -> Result<Response, AppError> {
    let browser_lang = crate::i18n::Lang::from_headers(&headers);
    if !app.cfg.auth.oidc() {
        return Err(AppError::NotFound);
    }
    let oidc = oidc(&app).await?;
    if let Some(e) = p.error {
        return Err(AppError::Forbidden(format!("Login aborted: {e}")));
    }
    let state: LoginState = jar
        .get(LOGIN_COOKIE)
        .and_then(|c| serde_json::from_str(c.value()).ok())
        .ok_or_else(|| AppError::BadRequest("Login expired, please start again.".into()))?;
    let jar = jar.remove(Cookie::build(LOGIN_COOKIE).path("/"));
    if p.state.as_deref() != Some(state.csrf.as_str()) {
        return Err(AppError::BadRequest("Invalid state parameter.".into()));
    }
    let code = p.code.ok_or_else(|| AppError::BadRequest("No code.".into()))?;

    let token = oidc
        .client
        .exchange_code(AuthorizationCode::new(code))
        .map_err(|e| anyhow!(e))?
        .set_pkce_verifier(PkceCodeVerifier::new(state.pkce))
        .request_async(&oidc.http)
        .await
        .map_err(|e| anyhow!("Token request failed: {e}"))?;
    let id_token = token.id_token().ok_or_else(|| anyhow!("No ID token"))?;
    let claims = id_token
        .claims(&oidc.client.id_token_verifier(), &Nonce::new(state.nonce))
        .map_err(|e| anyhow!("Invalid ID token: {e}"))?;

    let sub = claims.subject().to_string();
    // reserved prefixes of local and automation sessions
    if sub.starts_with(LOCAL_SUB) || sub.starts_with("automation:") {
        return Err(AppError::Forbidden("This account cannot be used here.".into()));
    }
    let email = claims.email().map(|e| e.to_string());
    let name = claims
        .name()
        .and_then(|n| n.get(None))
        .map(|n| n.to_string())
        .or_else(|| claims.preferred_username().map(|u| u.to_string()))
        .unwrap_or_else(|| sub.clone());
    // Signature is verified above; the groups are an additional claim in the same token,
    // otherwise in the access token.
    let mut groups = groups_from(jwt_claim(&id_token.to_string(), "groups"));
    if groups.is_empty() {
        groups = groups_from(jwt_claim(token.access_token().secret(), "groups"));
    }

    let cfg = &app.cfg;
    // Phone permission from the group only with the Keycloak sync; otherwise the admin sets it.
    // (self-service provisioning: no group needed)
    let phone = cfg.kc_sync.then(|| !cfg.provision.needs_phone_group() || groups.contains(&cfg.group_phone));
    let mut user_id = db::link_user(&app.db, &sub, email.as_deref(), phone, &groups).await?;
    if user_id.is_none() {
        user_id = crate::provision::at_login(&app, &sub, &name, email.as_deref(), &groups).await?;
    }
    let (web_access, web_admin): (bool, bool) = match user_id {
        Some(uid) => sqlx::query_as("SELECT web_access, web_admin FROM users WHERE id = $1")
            .bind(uid)
            .fetch_one(&app.db)
            .await
            .map_err(anyhow::Error::from)?,
        None => (false, false),
    };
    if !permissions(cfg, &groups, web_access, web_admin).1 {
        return Err(AppError::Forbidden(format!(
            "Your account is not in the group {} – please contact an admin.",
            cfg.group_webui
        )));
    }

    // UI language: stored per user; first login takes the Keycloak locale (or the browser language)
    let locale = claims
        .locale()
        .map(|l| l.as_str().to_lowercase())
        .and_then(|l| crate::i18n::Lang::parse(&l[..l.len().min(2)]))
        .unwrap_or(browser_lang);
    let lang = match user_id {
        Some(uid) => db::ui_language(&app.db, uid, locale).await?,
        None => locale,
    };
    // only the groups that matter here go into the cookie (size limit ~4 KB)
    let relevant = [&cfg.group_admin, &cfg.group_webui, &cfg.group_phone];
    let session = Session {
        sub,
        name,
        email,
        groups: groups.into_iter().filter(|g| relevant.contains(&g)).collect(),
        user_id,
        exp: session_exp(),
        pwv: None,
    };
    let jar = start_session(jar, &session, lang)?;
    Ok((jar, Redirect::to("/")).into_response())
}

pub async fn logout(jar: PrivateCookieJar) -> impl IntoResponse {
    (jar.remove(Cookie::build(SESSION_COOKIE).path("/")), Redirect::to("/auth/bye"))
}
