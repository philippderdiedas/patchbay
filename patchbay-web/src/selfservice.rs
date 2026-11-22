// SPDX-License-Identifier: GPL-3.0-or-later
//! Self-service do-not-disturb and call forwarding, used by the web UI and by the feature codes
//! (*761/*760, *21*<number>#, #21#) through the internal listener (127.0.0.1:8081).

use axum::{
    Form, Router,
    extract::{Path, State},
    response::{IntoResponse, Redirect, Response},
    routing::post,
};
use serde::Deserialize;

use crate::{
    AppError, AppState,
    admin::{normalise, to_e164},
    auth::CurrentUser,
    db,
};

/// Toggles or sets DND; returns the new state.
pub async fn set_dnd(app: &AppState, uid: i32, on: Option<bool>) -> anyhow::Result<bool> {
    let state: bool = sqlx::query_scalar("UPDATE users SET dnd = COALESCE($2, NOT dnd), updated_at = now() WHERE id = $1 RETURNING dnd")
        .bind(uid)
        .bind(on)
        .fetch_one(&app.db)
        .await?;
    db::audit(&app.db, &format!("user-id:{uid}"), "user.dnd", &format!("user-id:{uid}"), serde_json::json!({"dnd": state})).await?;
    Ok(state)
}

/// Why a forward was refused (shown in the UI; the phone just hears an error tone).
#[derive(Debug)]
pub enum ForwardError {
    Invalid(&'static str),
    NotAllowed,
}

/// A resolved call target (row of `targets`).
struct Resolved {
    kind: &'static str,
    user_id: Option<i32>,
    group_id: Option<i32>,
    ivr_id: Option<i32>,
    external: Option<String>,
}

/// Resolves a number typed by the user. Internal targets: persons, call groups, IVRs with a
/// number (number registry). External numbers must be allowed by the user's own dial rules
/// (SQL may_dial: rule dials out over a working trunk, its class is one of the user's classes).
/// The target records the user as owner, so the dialplan checks the same at call time.
async fn resolve_target(app: &AppState, uid: i32, raw: &str) -> Result<Resolved, ForwardError> {
    let db = &app.db;
    let err = |_| ForwardError::Invalid("database error");
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    let own: i16 = sqlx::query_scalar("SELECT extension FROM users WHERE id = $1").bind(uid).fetch_one(db).await.map_err(err)?;
    // internal: 4 digits from the number registry (persons, call groups, IVRs)
    let internal = !raw.starts_with('+') && !digits.starts_with('0') && digits.len() == 4;
    if internal {
        if digits == own.to_string() {
            return Err(ForwardError::Invalid("forwarding to yourself"));
        }
        let entry: Option<(String, i32)> =
            sqlx::query_as("SELECT kind, ref_id FROM numbers WHERE number::text = $1").bind(&digits).fetch_optional(db).await.map_err(err)?;
        let r = |kind, user_id, group_id, ivr_id| Resolved { kind, user_id, group_id, ivr_id, external: None };
        return match entry {
            Some((k, id)) if k == "person" => Ok(r("user", Some(id), None, None)),
            Some((k, id)) if k == "group" => Ok(r("group", None, Some(id), None)),
            Some((k, id)) if k == "ivr" => Ok(r("ivr", None, None, Some(id))),
            Some(_) => Err(ForwardError::Invalid("not a forwarding target")),
            None => Err(ForwardError::Invalid("unknown internal number")),
        };
    }
    let e164 = to_e164(raw).ok_or(ForwardError::Invalid("not a valid phone number"))?;
    // the dialplan passes the E.164 target through sub-outbound, which normalises it the same way
    let allowed: bool =
        sqlx::query_scalar("SELECT may_dial($1, $2)").bind(uid).bind(normalise(&e164)).fetch_one(db).await.map_err(err)?;
    if !allowed {
        return Err(ForwardError::NotAllowed);
    }
    Ok(Resolved { kind: "external", user_id: None, group_id: None, ivr_id: None, external: Some(e164) })
}

/// Inserts a self-service target owned by `uid`.
async fn insert_target(tx: &mut sqlx::PgConnection, uid: i32, t: &Resolved) -> Result<i32, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO targets (kind, user_id, group_id, ivr_id, external_e164, owner_user_id)
         VALUES ($1::target_kind, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(t.kind)
    .bind(t.user_id)
    .bind(t.group_id)
    .bind(t.ivr_id)
    .bind(&t.external)
    .bind(uid)
    .fetch_one(tx)
    .await
}

/// Deletes a replaced target unless something else still points at it (any FK to targets
/// refuses the delete; a savepoint keeps the surrounding transaction usable).
async fn drop_target(tx: &mut sqlx::PgConnection, old: Option<i32>) -> Result<(), sqlx::Error> {
    let Some(old) = old else { return Ok(()) };
    let mut sp = sqlx::Connection::begin(&mut *tx).await?;
    match sqlx::query("DELETE FROM targets WHERE id = $1").bind(old).execute(&mut *sp).await {
        Ok(_) => sp.commit().await,
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23503") => sp.rollback().await,
        Err(e) => Err(e),
    }
}

/// Sets (Some) or clears (None) the immediate forward of a user (targets: see resolve_target).
/// The previous target row is deleted when nothing else uses it.
pub async fn set_forward(app: &AppState, uid: i32, target: Option<&str>) -> Result<(), ForwardError> {
    let db = &app.db;
    let err = |_| ForwardError::Invalid("database error");
    let raw = target.map(str::trim).filter(|t| !t.is_empty());
    let t = match raw {
        Some(raw) => Some(resolve_target(app, uid, raw).await?),
        None => None,
    };
    let mut tx = db.begin().await.map_err(err)?;
    let old: Option<i32> = sqlx::query_scalar("SELECT forward_immediate FROM users WHERE id = $1 FOR UPDATE")
        .bind(uid)
        .fetch_one(&mut *tx)
        .await
        .map_err(err)?;
    let tid = match &t {
        Some(t) => Some(insert_target(&mut tx, uid, t).await.map_err(err)?),
        None => None,
    };
    sqlx::query("UPDATE users SET forward_immediate = $2, updated_at = now() WHERE id = $1").bind(uid).bind(tid).execute(&mut *tx).await.map_err(err)?;
    drop_target(&mut tx, old).await.map_err(err)?;
    tx.commit().await.map_err(err)?;
    let _ = db::audit(db, &format!("user-id:{uid}"), "user.forward", &format!("user-id:{uid}"), serde_json::json!({"target": raw})).await;
    Ok(())
}

// --- When I don't answer (PBX_USER_NOANSWER) ---------------------------------------------------

/// What happens after the ring time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoAnswer {
    /// the user's own mailbox
    Mailbox,
    /// forward to a number (checked like the immediate forward)
    Number(String),
    /// nothing: "nobody available" announcement
    Nothing,
}

/// Self-service ring time limits (admins may set 5–300 s)
pub const RING_MIN: i16 = 10;
pub const RING_MAX: i16 = 120;

/// Sets the no-answer target and/or the ring time of a user. Refused when the site does not allow
/// it (PBX_USER_NOANSWER=false).
pub async fn set_noanswer(app: &AppState, uid: i32, what: Option<NoAnswer>, ring_s: Option<i16>) -> Result<(), ForwardError> {
    if !app.cfg.user_noanswer {
        return Err(ForwardError::Invalid("set by the admins at this site"));
    }
    let db = &app.db;
    let err = |_| ForwardError::Invalid("database error");
    if let Some(r) = ring_s {
        if !(RING_MIN..=RING_MAX).contains(&r) {
            return Err(ForwardError::Invalid("ring time must be 10–120 seconds"));
        }
    }
    let target = match &what {
        None | Some(NoAnswer::Nothing) => None,
        Some(NoAnswer::Mailbox) => Some(Resolved { kind: "mailbox_user", user_id: Some(uid), group_id: None, ivr_id: None, external: None }),
        Some(NoAnswer::Number(raw)) => Some(resolve_target(app, uid, raw.trim()).await?),
    };
    let mut tx = db.begin().await.map_err(err)?;
    if what.is_some() {
        let old: Option<i32> = sqlx::query_scalar("SELECT no_answer_target FROM users WHERE id = $1 FOR UPDATE")
            .bind(uid)
            .fetch_one(&mut *tx)
            .await
            .map_err(err)?;
        let tid = match &target {
            Some(t) => Some(insert_target(&mut tx, uid, t).await.map_err(err)?),
            None => None,
        };
        sqlx::query("UPDATE users SET no_answer_target = $2, updated_at = now() WHERE id = $1").bind(uid).bind(tid).execute(&mut *tx).await.map_err(err)?;
        drop_target(&mut tx, old).await.map_err(err)?;
    }
    if let Some(r) = ring_s {
        sqlx::query("UPDATE users SET ring_timeout_s = $2, updated_at = now() WHERE id = $1").bind(uid).bind(r).execute(&mut *tx).await.map_err(err)?;
    }
    tx.commit().await.map_err(err)?;
    let shown = match &what {
        Some(NoAnswer::Mailbox) => Some("mailbox".to_string()),
        Some(NoAnswer::Number(n)) => Some(n.clone()),
        Some(NoAnswer::Nothing) => Some("nothing".to_string()),
        None => None,
    };
    let _ = db::audit(db, &format!("user-id:{uid}"), "user.noanswer", &format!("user-id:{uid}"), serde_json::json!({"target": shown, "ring_s": ring_s})).await;
    Ok(())
}

/// Current no-answer setting for UI and bot.
pub struct NoAnswerState {
    /// "mailbox", "number", "nothing" or "other" (set by an admin, e.g. an announcement)
    pub mode: &'static str,
    /// number for "number", description for "other"
    pub value: Option<String>,
    pub ring_s: i16,
}

impl NoAnswerState {
    /// ring time choices for the form: (seconds, selected); keeps an admin-set value
    pub fn ring_options(&self) -> Vec<(i16, bool)> {
        let mut v: Vec<i16> = vec![10, 15, 20, 25, 30, 45, 60, 90, 120];
        if !v.contains(&self.ring_s) {
            v.push(self.ring_s);
            v.sort();
        }
        v.into_iter().map(|s| (s, s == self.ring_s)).collect()
    }
}

pub async fn noanswer_state(app: &AppState, uid: i32) -> anyhow::Result<NoAnswerState> {
    let row: (i16, Option<String>, Option<i32>, Option<String>) = sqlx::query_as(
        "SELECT u.ring_timeout_s, t.kind::text, t.user_id,
                COALESCE(tu.extension::text, tg.number::text, ti.number::text, t.external_e164)
         FROM users u LEFT JOIN targets t ON t.id = u.no_answer_target
         LEFT JOIN users tu ON tu.id = t.user_id LEFT JOIN groups tg ON tg.id = t.group_id LEFT JOIN ivrs ti ON ti.id = t.ivr_id
         WHERE u.id = $1",
    )
    .bind(uid)
    .fetch_one(&app.db)
    .await?;
    let (ring_s, kind, tuser, value) = row;
    let (mode, value) = match kind.as_deref() {
        None => ("nothing", None),
        Some("mailbox_user") if tuser == Some(uid) => ("mailbox", None),
        Some("user" | "group" | "ivr" | "external") => ("number", value),
        Some(k) => ("other", Some(match k {
            "mailbox_user" => format!("mailbox {}", value.unwrap_or_default()),
            "mailbox_group" => format!("group mailbox {}", value.unwrap_or_default()),
            "announcement" => "announcement".to_string(),
            "hangup" => "hang up".to_string(),
            other => other.to_string(),
        })),
    };
    Ok(NoAnswerState { mode, value, ring_s })
}

/// Current forward as text for the UI ("21", "512", "+49176…")
pub async fn forward_text(app: &AppState, uid: i32) -> anyhow::Result<Option<String>> {
    Ok(forward_target(app, uid).await?.map(|(n, _)| n))
}

/// Current forward: (number as in forward_text, name of an internal target: person, call group
/// or IVR).
pub async fn forward_target(app: &AppState, uid: i32) -> anyhow::Result<Option<(String, Option<String>)>> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT COALESCE(tu.extension::text, tg.number::text, ti.number::text, t.external_e164),
                COALESCE(tu.display_name, tg.name, ti.name)
         FROM users u JOIN targets t ON t.id = u.forward_immediate
         LEFT JOIN users tu ON tu.id = t.user_id LEFT JOIN groups tg ON tg.id = t.group_id LEFT JOIN ivrs ti ON ti.id = t.ivr_id
         WHERE u.id = $1",
    )
    .bind(uid)
    .fetch_optional(&app.db)
    .await?;
    Ok(row.and_then(|(n, name)| n.map(|n| (n, name))))
}

// --- Web UI -------------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct DndForm {
    #[serde(default)]
    on: Option<String>,
}

pub async fn ui_dnd(State(app): State<AppState>, u: CurrentUser, Form(f): Form<DndForm>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    set_dnd(&app, uid, Some(f.on.is_some())).await?;
    Ok(Redirect::to("/#dnd").into_response())
}

/// Call groups of a person for the start page (log in/out)
#[derive(sqlx::FromRow)]
pub struct MyGroup {
    pub id: i32,
    pub number: i16,
    pub name: String,
    pub queue: bool,
    pub logged_in: bool,
}

pub async fn my_groups(app: &AppState, uid: i32) -> anyhow::Result<Vec<MyGroup>> {
    Ok(sqlx::query_as(
        "SELECT g.id, g.number, g.name, g.queue, m.logged_in FROM group_members m JOIN groups g ON g.id = m.group_id
         WHERE m.user_id = $1 AND m.enabled AND g.deleted_at IS NULL ORDER BY g.number",
    )
    .bind(uid)
    .fetch_all(&app.db)
    .await?)
}

pub async fn ui_group_login(
    State(app): State<AppState>,
    u: CurrentUser,
    Path(gid): Path<i32>,
    Form(f): Form<crate::admin::LoginForm>,
) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    if !crate::admin::set_logged_in(&app.db, &u.session.sub, gid, uid, f.on.is_some()).await? {
        return Err(AppError::NotFound);
    }
    Ok(Redirect::to("/#groups").into_response())
}

#[derive(Deserialize)]
pub struct NoAnswerForm {
    /// mailbox | number | nothing
    mode: String,
    #[serde(default)]
    number: String,
    ring_s: i16,
}

pub async fn ui_noanswer(State(app): State<AppState>, u: CurrentUser, Form(f): Form<NoAnswerForm>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    if !app.cfg.user_noanswer {
        return Err(AppError::Forbidden("The ring time and the no-answer target are set by the admins at this site.".into()));
    }
    let what = match f.mode.as_str() {
        "mailbox" => NoAnswer::Mailbox,
        "number" => NoAnswer::Number(f.number.clone()),
        _ => NoAnswer::Nothing,
    };
    set_noanswer(&app, uid, Some(what), Some(f.ring_s)).await.map_err(|e| match e {
        ForwardError::Invalid(why) => AppError::BadRequest(format!("Not saved: {why}.")),
        ForwardError::NotAllowed => AppError::BadRequest("Not saved: you may not call this number.".into()),
    })?;
    Ok(Redirect::to("/#noanswer").into_response())
}

#[derive(Deserialize)]
pub struct ForwardForm {
    #[serde(default)]
    target: String,
    #[serde(default)]
    clear: Option<String>,
}

pub async fn ui_forward(State(app): State<AppState>, u: CurrentUser, Form(f): Form<ForwardForm>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let target = if f.clear.is_some() { None } else { Some(f.target.as_str()) };
    set_forward(&app, uid, target).await.map_err(|e| match e {
        ForwardError::Invalid(why) => AppError::BadRequest(format!("Forwarding not set: {why}.")),
        ForwardError::NotAllowed => AppError::BadRequest("Forwarding not set: you may not call this number.".into()),
    })?;
    Ok(Redirect::to("/#forwarding").into_response())
}

// --- Feature codes (internal listener, called by the dialplan via CURL) ---------------------

pub fn internal_router(app: AppState) -> Router {
    Router::new()
        .route("/internal/dnd", post(fc_dnd))
        .route("/internal/forward", post(fc_forward))
        .with_state(app)
}

#[derive(Deserialize)]
struct FcForm {
    ext: String,
    #[serde(default)]
    target: String,
    /// DND: "1" = on, "0" = off
    #[serde(default)]
    on: String,
}

/// Enabled, not deleted user with this extension (feature codes, identified by the calling device).
pub(crate) async fn uid_by_ext(app: &AppState, ext: &str) -> Option<i32> {
    sqlx::query_scalar("SELECT id FROM users WHERE extension::text = $1 AND deleted_at IS NULL AND enabled")
        .bind(ext)
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten()
}

/// *761 / *760: DND on/off → "on" / "off" / "error"
async fn fc_dnd(State(app): State<AppState>, Form(f): Form<FcForm>) -> &'static str {
    let on = match f.on.as_str() {
        "1" => true,
        "0" => false,
        _ => return "error",
    };
    match uid_by_ext(&app, &f.ext).await {
        Some(uid) => match set_dnd(&app, uid, Some(on)).await {
            Ok(true) => "on",
            Ok(false) => "off",
            Err(_) => "error",
        },
        None => "error",
    }
}

/// *21*<number># sets, #21# (empty target) clears → "set" / "cleared" / "denied" / "invalid"
async fn fc_forward(State(app): State<AppState>, Form(f): Form<FcForm>) -> &'static str {
    let Some(uid) = uid_by_ext(&app, &f.ext).await else { return "invalid" };
    let target = Some(f.target.as_str()).filter(|t| !t.is_empty());
    match set_forward(&app, uid, target).await {
        Ok(()) if target.is_some() => "set",
        Ok(()) => "cleared",
        Err(ForwardError::NotAllowed) => "denied",
        Err(_) => "invalid",
    }
}
