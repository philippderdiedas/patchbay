// SPDX-License-Identifier: GPL-3.0-or-later
//! Local logins (PBX_AUTH=local|both): login name + argon2id password hash in `local_logins`.
//!
//! * Session subject `local:<users.id>`, password version in the session (a password change or
//!   reset ends all other sessions of that login).
//! * Failed attempts lock the login name and the client IP for a short, growing time (`Limits`).
//! * Unknown login names cost the same argon2 verification as known ones.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use argon2::{
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
    password_hash::SaltString,
};
use askama::Template;
use axum::{
    Form,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::PrivateCookieJar;
use serde::Deserialize;

use crate::{
    AppError, AppState,
    auth::{self, Admin, CurrentUser, LOCAL_SUB, Session},
    db,
    i18n::Lang,
    ui::Ctx,
};

pub const MIN_PASSWORD: usize = 10;
pub const MAX_PASSWORD: usize = 256;

// --- Passwords -------------------------------------------------------------------------

/// argon2id with the crate defaults (19 MiB, 2 iterations, 1 lane), PHC string format.
pub fn hash_password(pw: &str) -> Result<String> {
    let mut salt = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut salt);
    let salt = SaltString::encode_b64(&salt).map_err(|e| anyhow!("salt: {e}"))?;
    Ok(Argon2::default().hash_password(pw.as_bytes(), &salt).map_err(|e| anyhow!("hash: {e}"))?.to_string())
}

/// Verifies against a PHC string (parameters are taken from the hash).
pub fn verify_password(pw: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
}

/// Hash of a random password: verified for unknown login names so they take as long as known ones.
static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
    let mut pw = [0u8; 24];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut pw);
    hash_password(&hex::encode(pw)).expect("argon2 dummy hash")
});

/// At most a few argon2 runs (19 MiB each) at the same time.
static HASH_SLOTS: LazyLock<tokio::sync::Semaphore> = LazyLock::new(|| tokio::sync::Semaphore::new(4));

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    let _slot = HASH_SLOTS.acquire().await?;
    Ok(tokio::task::spawn_blocking(f).await?)
}

async fn verify_async(pw: String, hash: Option<String>) -> Result<bool> {
    blocking(move || {
        let known = hash.is_some();
        let h = hash.unwrap_or_else(|| DUMMY_HASH.clone());
        // overlong input: still spend the time, never succeed
        let too_long = pw.len() > 4 * MAX_PASSWORD;
        let ok = verify_password(if too_long { "" } else { &pw }, &h);
        ok && known && !too_long
    })
    .await
}

async fn hash_async(pw: String) -> Result<String> {
    blocking(move || hash_password(&pw)).await?
}

/// Password rules (length only; NIST SP 800-63B style).
pub fn check_password(pw: &str) -> Result<(), &'static str> {
    let n = pw.chars().count();
    if n < MIN_PASSWORD {
        Err("The password needs at least 10 characters.")
    } else if n > MAX_PASSWORD {
        Err("The password is too long (at most 256 characters).")
    } else {
        Ok(())
    }
}

/// Login names: 2–64 characters a–z 0–9 . _ - @, starting with a letter or digit; stored lowercase.
pub fn normalize_login(s: &str) -> Option<String> {
    let l = s.trim().to_lowercase();
    let ok = (2..=64).contains(&l.len())
        && l.starts_with(|c: char| c.is_ascii_alphanumeric())
        && l.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || ".-_@".contains(c));
    ok.then_some(l)
}

const LOGIN_RULES: &str = "Login name: 2–64 characters a–z, 0–9, . _ - @, starting with a letter or digit.";

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505")
}

// --- Lockout -------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct Policy {
    /// failures within `window` that trigger a lockout
    pub max_failures: u32,
    pub window: Duration,
    /// first lockout; doubles with every further lockout up to `max_lock`
    pub lock: Duration,
    pub max_lock: Duration,
}

#[derive(Debug)]
struct Entry {
    failures: u32,
    first: Instant,
    last: Instant,
    locked_until: Option<Instant>,
    lockouts: u32,
}

/// Counts failed attempts per key (login name or IP), in memory (reset on restart).
#[derive(Debug)]
pub struct Limiter {
    policy: Policy,
    map: Mutex<HashMap<String, Entry>>,
}

/// After a day without failures a key starts from scratch (also for the lockout escalation).
const FORGET: Duration = Duration::from_secs(24 * 3600);
const MAX_KEYS: usize = 10_000;

impl Limiter {
    pub fn new(policy: Policy) -> Self {
        Limiter { policy, map: Mutex::new(HashMap::new()) }
    }

    /// Remaining lock time, None if the key may try.
    pub fn locked(&self, key: &str, now: Instant) -> Option<Duration> {
        let map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        map.get(key).and_then(|e| e.locked_until).filter(|u| *u > now).map(|u| u - now)
    }

    /// Records a failure; returns the lock time if this failure triggered a lockout.
    pub fn fail(&self, key: &str, now: Instant) -> Option<Duration> {
        let p = self.policy;
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= MAX_KEYS {
            map.retain(|_, e| e.locked_until.is_some_and(|u| u > now) || now.duration_since(e.last) < p.window);
        }
        let e = map.entry(key.to_string()).or_insert(Entry { failures: 0, first: now, last: now, locked_until: None, lockouts: 0 });
        if now.duration_since(e.last) >= FORGET {
            e.lockouts = 0;
        }
        if now.duration_since(e.first) >= p.window || e.locked_until.is_some_and(|u| u <= now) {
            e.failures = 0;
            e.first = now;
            e.locked_until = None;
        }
        e.failures += 1;
        e.last = now;
        if e.failures >= p.max_failures {
            let lock = p.lock.saturating_mul(1u32 << e.lockouts.min(16)).min(p.max_lock);
            e.locked_until = Some(now + lock);
            e.lockouts += 1;
            e.failures = 0;
            return Some(lock);
        }
        None
    }

    /// Successful login: forget the failures of this key.
    pub fn clear(&self, key: &str) {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
    }
}

/// Lockout state of local logins.
#[derive(Debug)]
pub struct Limits {
    /// 5 failures in 15 min lock the login name for 5 min (10, 20, … up to 1 h)
    pub by_login: Limiter,
    /// 20 failures in 15 min lock the client IP for 15 min (up to 1 h)
    pub by_ip: Limiter,
}

impl Default for Limits {
    fn default() -> Self {
        let min = |m: u64| Duration::from_secs(m * 60);
        Limits {
            by_login: Limiter::new(Policy { max_failures: 5, window: min(15), lock: min(5), max_lock: min(60) }),
            by_ip: Limiter::new(Policy { max_failures: 20, window: min(15), lock: min(15), max_lock: min(60) }),
        }
    }
}

impl Limits {
    fn locked(&self, login: &str, ip: &str) -> bool {
        let now = Instant::now();
        self.by_ip.locked(ip, now).is_some() || self.by_login.locked(login, now).is_some()
    }

    fn fail(&self, login: &str, ip: &str) {
        let now = Instant::now();
        if let Some(d) = self.by_login.fail(login, now) {
            tracing::warn!("local login {login:?} locked for {} min", d.as_secs() / 60);
        }
        if let Some(d) = self.by_ip.fail(ip, now) {
            tracing::warn!("local login from {ip} locked for {} min", d.as_secs() / 60);
        }
    }
}

/// Key for the per-login limiter (also for names that do not exist).
fn login_key(s: &str) -> String {
    s.trim().to_lowercase().chars().take(64).collect()
}

const MSG_FAILED: &str = "Login failed: wrong login name or password.";
const MSG_LOCKED: &str = "Too many failed attempts. Please wait a few minutes and try again.";

// --- Login page ------------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    c: Ctx,
    local: bool,
    oidc: bool,
    login: String,
    error: Option<&'static str>,
}

fn login_page_status(app: &AppState, headers: &HeaderMap, login: &str, error: Option<&'static str>, status: StatusCode) -> Result<Response, AppError> {
    let page = LoginPage {
        c: Ctx::anonymous(Lang::from_headers(headers)),
        local: app.cfg.auth.local(),
        oidc: app.cfg.auth.oidc(),
        login: login.chars().take(64).collect(),
        error,
    };
    Ok((status, Html(page.render().map_err(anyhow::Error::from)?)).into_response())
}

/// GET /auth/login with PBX_AUTH=local|both.
pub fn login_page(app: &AppState, headers: &HeaderMap, error: Option<&'static str>) -> Result<Response, AppError> {
    login_page_status(app, headers, "", error, StatusCode::OK)
}

#[derive(Deserialize)]
pub struct LoginForm {
    #[serde(default)]
    login: String,
    #[serde(default)]
    password: String,
}

/// POST /auth/login (CSRF: same_origin middleware like every POST).
pub async fn login_submit(
    State(app): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    jar: PrivateCookieJar,
    headers: HeaderMap,
    Form(f): Form<LoginForm>,
) -> Result<Response, AppError> {
    if !app.cfg.auth.local() {
        return Err(AppError::NotFound);
    }
    let key = login_key(&f.login);
    let ip = crate::proxy::client_ip(&app.cfg.trusted_proxies, addr, &headers).to_string();
    if app.login_limits.locked(&key, &ip) {
        return login_page_status(&app, &headers, &f.login, Some(MSG_LOCKED), StatusCode::TOO_MANY_REQUESTS);
    }
    let row: Option<(i32, String, String, Option<String>, bool, bool, i64)> = match normalize_login(&f.login) {
        Some(login) => sqlx::query_as(
            "SELECT l.user_id, l.password_hash, u.display_name, u.email, u.enabled AND u.deleted_at IS NULL,
                    u.web_access OR u.web_admin, (extract(epoch FROM l.password_changed_at) * 1000)::bigint
             FROM local_logins l JOIN users u ON u.id = l.user_id WHERE l.login = $1",
        )
        .bind(login)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?,
        None => None,
    };
    let ok = verify_async(f.password, row.as_ref().map(|r| r.1.clone())).await?;
    let Some((uid, _, name, email, active, web, pwv)) = row.filter(|_| ok) else {
        app.login_limits.fail(&key, &ip);
        tracing::info!("local login failed for {key:?} from {ip}");
        return login_page_status(&app, &headers, &f.login, Some(MSG_FAILED), StatusCode::UNAUTHORIZED);
    };
    app.login_limits.by_login.clear(&key);
    // only reported after a correct password
    if !active {
        return Err(AppError::Forbidden("Your phone account is no longer active.".into()));
    }
    if !web {
        return Err(AppError::Forbidden("No permission for the phone system.".into()));
    }
    let lang = db::ui_language(&app.db, uid, Lang::from_headers(&headers)).await?;
    let session = Session {
        sub: format!("{LOCAL_SUB}{uid}"),
        name,
        email,
        groups: vec![],
        user_id: Some(uid),
        exp: auth::session_exp(),
        pwv: Some(pwv),
    };
    tracing::info!("local login {key:?} (user {uid}) from {ip}");
    let jar = auth::start_session(jar, &session, lang)?;
    Ok((jar, Redirect::to("/")).into_response())
}

// --- Own password ----------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct OwnPassword {
    #[serde(default)]
    current: String,
    #[serde(default)]
    new: String,
    #[serde(default)]
    repeat: String,
}

/// Updates the hash; returns the new password version.
async fn store_password(db: &sqlx::PgPool, uid: i32, hash: &str) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "UPDATE local_logins SET password_hash = $2, password_changed_at = clock_timestamp() WHERE user_id = $1
         RETURNING (extract(epoch FROM password_changed_at) * 1000)::bigint",
    )
    .bind(uid)
    .bind(hash)
    .fetch_one(db)
    .await?)
}

/// POST /me/password: change the own local password (current password required).
pub async fn change_own(
    State(app): State<AppState>,
    u: CurrentUser,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    jar: PrivateCookieJar,
    Form(f): Form<OwnPassword>,
) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let (login, hash): (String, String) = sqlx::query_as("SELECT login, password_hash FROM local_logins WHERE user_id = $1")
        .bind(uid)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or_else(|| AppError::BadRequest("You have no local login.".into()))?;
    let ip = crate::proxy::client_ip(&app.cfg.trusted_proxies, addr, &headers).to_string();
    if app.login_limits.locked(&login, &ip) {
        return Err(AppError::BadRequest(MSG_LOCKED.into()));
    }
    if !verify_async(f.current, Some(hash)).await? {
        app.login_limits.fail(&login, &ip);
        return Err(AppError::BadRequest("The current password is wrong.".into()));
    }
    if f.new != f.repeat {
        return Err(AppError::BadRequest("The new passwords do not match.".into()));
    }
    check_password(&f.new).map_err(|e| AppError::BadRequest(e.into()))?;
    let hash = hash_async(f.new).await?;
    let pwv = store_password(&app.db, uid, &hash).await?;
    db::audit(&app.db, &u.session.sub, "login.password", &format!("user-id:{uid}"), serde_json::json!({"login": login})).await?;
    // all other sessions of this login end; keep the current one
    let jar = if u.session.is_local() {
        let mut s = u.session.clone();
        s.pwv = Some(pwv);
        auth::start_session(jar, &s, u.lang)?
    } else {
        jar
    };
    Ok((jar, Redirect::to("/?password=changed")).into_response())
}

// --- Admin ---------------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct AdminLoginForm {
    #[serde(default)]
    login: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    action: String,
}

/// POST /admin/users/{id}/login: create a local login, rename it, set a new password, or delete it.
pub async fn admin_save(
    State(app): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<i32>,
    jar: PrivateCookieJar,
    Form(f): Form<AdminLoginForm>,
) -> Result<Response, AppError> {
    let exists: Option<i32> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1 AND deleted_at IS NULL")
        .bind(id)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    exists.ok_or(AppError::NotFound)?;
    let own = a.session.sub == format!("{LOCAL_SUB}{id}");
    let back = Redirect::to(&format!("/admin/users/{id}/edit#login"));
    if f.action == "delete" {
        if own {
            return Err(AppError::BadRequest("You cannot remove the login you are using.".into()));
        }
        let gone: Option<String> = sqlx::query_scalar("DELETE FROM local_logins WHERE user_id = $1 RETURNING login")
            .bind(id)
            .fetch_optional(&app.db)
            .await
            .map_err(anyhow::Error::from)?;
        if let Some(login) = gone {
            db::audit(&app.db, &a.session.sub, "login.delete", &format!("user-id:{id}"), serde_json::json!({"login": login})).await?;
        }
        return Ok(back.into_response());
    }
    let login = normalize_login(&f.login).ok_or_else(|| AppError::BadRequest(LOGIN_RULES.into()))?;
    let hash = if f.password.is_empty() {
        None
    } else {
        check_password(&f.password).map_err(|e| AppError::BadRequest(e.into()))?;
        Some(hash_async(f.password).await?)
    };
    let taken = || AppError::BadRequest(format!("The login name {login} is already taken."));
    let existing: Option<String> = sqlx::query_scalar("SELECT login FROM local_logins WHERE user_id = $1")
        .bind(id)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let mut pwv = None;
    match (&existing, &hash) {
        (None, None) => return Err(AppError::BadRequest("A new login needs a password.".into())),
        (None, Some(h)) => {
            sqlx::query("INSERT INTO local_logins (user_id, login, password_hash) VALUES ($1, $2, $3)")
                .bind(id)
                .bind(&login)
                .bind(h)
                .execute(&app.db)
                .await
                .map_err(|e| if is_unique_violation(&e) { taken() } else { AppError::Internal(e.into()) })?;
        }
        (Some(_), h) => {
            sqlx::query("UPDATE local_logins SET login = $2 WHERE user_id = $1")
                .bind(id)
                .bind(&login)
                .execute(&app.db)
                .await
                .map_err(|e| if is_unique_violation(&e) { taken() } else { AppError::Internal(e.into()) })?;
            if let Some(h) = h {
                pwv = Some(store_password(&app.db, id, h).await?);
            }
        }
    }
    db::audit(
        &app.db,
        &a.session.sub,
        "login.set",
        &format!("user-id:{id}"),
        serde_json::json!({"login": login, "old_login": existing, "password_set": hash.is_some()}),
    )
    .await?;
    // an admin who resets their own password stays logged in
    let jar = match pwv {
        Some(v) if own => {
            let mut s = a.session.clone();
            s.pwv = Some(v);
            auth::start_session(jar, &s, a.lang)?
        }
        _ => jar,
    };
    Ok((jar, back).into_response())
}

// --- CLI -------------------------------------------------------------------------------------

/// `set-password --create`: creates the person `target` (4-digit extension or `auto`) unless a
/// person with that extension exists. Same defaults as Admin → Users → new (route classes).
/// Returns the extension.
async fn create_person(db: &sqlx::PgPool, target: &str, name: &str) -> Result<i16> {
    let name = name.trim();
    anyhow::ensure!(!name.is_empty() && name.chars().count() <= 100 && !name.chars().any(char::is_control), "--create: invalid name");
    let plan = crate::numbers::NumberPlan::from_env()?;
    let ext: i16 = if target == "auto" {
        match crate::numbers::allocate(db, &plan, "person").await {
            Ok(Some(n)) => n,
            Ok(None) => bail!("person numbers are switched off"),
            Err(e) => bail!("no free number: {e:?}"),
        }
    } else {
        anyhow::ensure!(target.len() == 4 && target.chars().all(|c| c.is_ascii_digit()), "--create needs a 4-digit extension or auto");
        let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE extension = $1 AND deleted_at IS NULL)")
            .bind(target.parse::<i16>()?)
            .fetch_one(db)
            .await?;
        if exists {
            return Ok(target.parse()?);
        }
        match crate::numbers::check(db, &plan, "person", target, None).await {
            Ok(n) => n,
            Err(e) => bail!("extension {target}: {e:?}"),
        }
    };
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO users (kind, extension, display_name, route_classes)
         VALUES ('person', $1, $2, ARRAY(SELECT key FROM route_classes WHERE default_on ORDER BY key)) RETURNING id",
    )
    .bind(ext)
    .bind(name)
    .fetch_one(db)
    .await
    .context("creating the user")?;
    db::audit(db, "system:cli", "user.create", &format!("user-id:{id}"), serde_json::json!({"extension": ext, "name": name})).await?;
    println!("User {ext} {name} created.");
    Ok(ext)
}

const USAGE: &str =
    "usage: patchbay-web set-password <extension|login|auto> [--login <name>] [--admin] [--create \"<name>\"]  (password on stdin)";

/// `patchbay-web set-password <extension|login> [--login <name>] [--admin] [--create "<name>"]`: creates
/// or updates the local login of a user. The new login name defaults to the extension. Grants web
/// access (`--admin`: also admin). Reads the password from the first line of stdin. With `--create`
/// a missing person is created first (extension `auto`: the next free number of the number plan),
/// so a new site gets its first admin without the web UI.
pub async fn cli_set_password(args: Vec<String>) -> Result<()> {
    let (mut target, mut login, mut admin, mut create) = (None, None, false, None);
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--admin" => admin = true,
            "--login" => login = Some(it.next().context(USAGE)?),
            "--create" => create = Some(it.next().context(USAGE)?),
            _ if target.is_none() && !a.starts_with('-') => target = Some(a),
            _ => bail!(USAGE),
        }
    }
    let target = target.context(USAGE)?;
    let login = match login {
        Some(l) => Some(normalize_login(&l).context(LOGIN_RULES)?),
        None => None,
    };
    let mut pw = String::new();
    std::io::stdin().read_line(&mut pw).context("reading the password from stdin")?;
    let pw = pw.trim_end_matches(['\r', '\n']).to_string();
    check_password(&pw).map_err(|e| anyhow!(e))?;

    let db = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&crate::config::database_url()?)
        .await
        .context("Database unreachable")?;
    let target = match &create {
        Some(name) => create_person(&db, &target, name).await?.to_string(),
        None => target,
    };
    let by_ext = target.len() == 4 && target.chars().all(|c| c.is_ascii_digit());
    let user: Option<(i32, i16, String, Option<String>)> = if by_ext {
        sqlx::query_as(
            "SELECT u.id, u.extension, u.display_name, l.login FROM users u LEFT JOIN local_logins l ON l.user_id = u.id
             WHERE u.extension = $1 AND u.deleted_at IS NULL",
        )
        .bind(target.parse::<i16>()?)
        .fetch_optional(&db)
        .await?
    } else {
        sqlx::query_as(
            "SELECT u.id, u.extension, u.display_name, l.login FROM users u JOIN local_logins l ON l.user_id = u.id
             WHERE l.login = $1 AND u.deleted_at IS NULL",
        )
        .bind(target.trim().to_lowercase())
        .fetch_optional(&db)
        .await?
    };
    let (uid, ext, name, existing) = user.with_context(|| format!("no user with extension or login name {target}"))?;
    let login = login.or(existing.clone()).unwrap_or_else(|| ext.to_string());
    let hash = hash_password(&pw)?;
    let mut tx = db.begin().await?;
    sqlx::query(
        "INSERT INTO local_logins (user_id, login, password_hash) VALUES ($1, $2, $3)
         ON CONFLICT (user_id) DO UPDATE SET login = EXCLUDED.login, password_hash = EXCLUDED.password_hash,
                                             password_changed_at = clock_timestamp()",
    )
    .bind(uid)
    .bind(&login)
    .bind(&hash)
    .execute(&mut *tx)
    .await
    .map_err(|e| if is_unique_violation(&e) { anyhow!("the login name {login} is already taken") } else { e.into() })?;
    sqlx::query("UPDATE users SET web_access = true, web_admin = web_admin OR $2, updated_at = now() WHERE id = $1")
        .bind(uid)
        .bind(admin)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    db::audit(
        &db,
        "system:cli",
        "login.set",
        &format!("user-id:{uid}"),
        serde_json::json!({"login": login, "old_login": existing, "password_set": true, "admin": admin}),
    )
    .await?;
    println!("Local login \"{login}\" set for {ext} {name}{}.", if admin { " (admin)" } else { "" });
    let mode = std::env::var("PBX_AUTH").unwrap_or_default().to_lowercase();
    if !matches!(mode.as_str(), "local" | "both") {
        eprintln!("Note: local logins are only accepted with PBX_AUTH=local or PBX_AUTH=both.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify() {
        let h = hash_password("correct horse battery").unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify_password("correct horse battery", &h));
        assert!(!verify_password("correct horse batterY", &h));
        assert!(!verify_password("", &h));
        assert!(!verify_password("x", "not a hash"));
        // random salt
        assert_ne!(h, hash_password("correct horse battery").unwrap());
    }

    #[tokio::test]
    async fn unknown_users_never_succeed() {
        let h = hash_password("0123456789").unwrap();
        assert!(verify_async("0123456789".into(), Some(h.clone())).await.unwrap());
        assert!(!verify_async("0123456789".into(), None).await.unwrap());
        assert!(!verify_async("x".repeat(5000), Some(h)).await.unwrap());
    }

    #[test]
    fn password_rules() {
        assert!(check_password("short").is_err());
        assert!(check_password("ääääääääää").is_ok());
        assert!(check_password(&"a".repeat(257)).is_err());
    }

    #[test]
    fn login_names() {
        assert_eq!(normalize_login(" Ada.L "), Some("ada.l".into()));
        assert_eq!(normalize_login("1001"), Some("1001".into()));
        assert_eq!(normalize_login("a@b.de"), Some("a@b.de".into()));
        assert_eq!(normalize_login("x"), None);
        assert_eq!(normalize_login(".abc"), None);
        assert_eq!(normalize_login("a b"), None);
        assert_eq!(normalize_login("müller"), None);
        assert_eq!(normalize_login(&"a".repeat(65)), None);
    }

    fn policy() -> Policy {
        let s = Duration::from_secs;
        Policy { max_failures: 3, window: s(60), lock: s(10), max_lock: s(25) }
    }

    #[test]
    fn locks_after_max_failures_and_escalates() {
        let l = Limiter::new(policy());
        let t = Instant::now();
        assert_eq!(l.fail("a", t), None);
        assert_eq!(l.fail("a", t), None);
        assert!(l.locked("a", t).is_none());
        assert_eq!(l.fail("a", t), Some(Duration::from_secs(10)));
        assert!(l.locked("a", t + Duration::from_secs(9)).is_some());
        assert!(l.locked("b", t).is_none());
        let t2 = t + Duration::from_secs(10);
        assert!(l.locked("a", t2).is_none());
        // second lockout: doubled
        l.fail("a", t2);
        l.fail("a", t2);
        assert_eq!(l.fail("a", t2), Some(Duration::from_secs(20)));
        // third: capped
        let t3 = t2 + Duration::from_secs(20);
        l.fail("a", t3);
        l.fail("a", t3);
        assert_eq!(l.fail("a", t3), Some(Duration::from_secs(25)));
    }

    #[test]
    fn failures_expire_and_clear() {
        let l = Limiter::new(policy());
        let t = Instant::now();
        l.fail("a", t);
        l.fail("a", t);
        // window over: counting starts again
        let t2 = t + Duration::from_secs(61);
        assert_eq!(l.fail("a", t2), None);
        assert_eq!(l.fail("a", t2), None);
        l.clear("a");
        assert_eq!(l.fail("a", t2), None);
        assert_eq!(l.fail("a", t2), None);
        assert!(l.fail("a", t2).is_some());
        // after a day without failures the escalation is forgotten
        let t3 = t2 + FORGET;
        l.fail("a", t3);
        l.fail("a", t3);
        assert_eq!(l.fail("a", t3), Some(Duration::from_secs(10)));
    }
}
