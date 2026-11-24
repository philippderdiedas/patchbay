// SPDX-License-Identifier: GPL-3.0-or-later
//! Admin pages: edit users, numbers, status, call log, audit log.

use askama::Template;
use axum::{
    Form,
    extract::{Path, State},
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use sqlx::{FromRow, PgPool};

use axum_extra::extract::Form as MultiForm;

use crate::{
    AppError, AppState, ami,
    auth::Admin,
    db,
    ui::{Ctx, TARGET_DESC, TARGET_JOINS, TargetOptions, TargetSel, render, target_desc, target_joins, target_value},
};

/// Country calling code and emergency numbers of the site (PBX_COUNTRY_CODE, PBX_EMERGENCY_NUMBERS),
/// set once at startup (main.rs), so number helpers deep in parsers need no AppState.
static DIALING: std::sync::OnceLock<(String, Vec<String>)> = std::sync::OnceLock::new();

pub fn init_dialing(country_code: &str, emergency: &[String]) {
    let _ = DIALING.set((country_code.to_string(), emergency.to_vec()));
}

fn dialing() -> (&'static str, &'static [String]) {
    DIALING.get().map(|(c, e)| (c.as_str(), e.as_slice())).unwrap_or(("", &[]))
}

/// The site's country calling code (PBX_COUNTRY_CODE, e.g. "49"; empty when unset).
pub fn country_code() -> &'static str {
    dialing().0
}

/// A dialled number in the form the dial rules match on, exactly as [sub-outbound] in
/// extensions.conf does it (keep both in sync); see `normalise_with`.
pub fn normalise(raw: &str) -> String {
    let (cc, emergency) = dialing();
    normalise_with(raw, cc, emergency)
}

/// Only digits and +; 00 → +; +<cc>0… → +<cc>… (the trunk 0 written after the country code);
/// +<cc>… → 0…; other countries + → 00; 0 + emergency number → emergency number (e.g. 0112 →
/// 112). Everything else (short numbers, site prefixes) stays as dialled. Assumes trunk prefix 0
/// and international prefix 00 (ITU recommendation, used by most countries).
pub fn normalise_with(raw: &str, cc: &str, emergency: &[String]) -> String {
    let mut n: String = raw.chars().filter(|c| c.is_ascii_digit() || *c == '+').collect();
    if let Some(r) = n.strip_prefix("00") {
        n = format!("+{r}");
    }
    if !cc.is_empty() {
        if let Some(r) = n.strip_prefix(&format!("+{cc}0")) {
            n = format!("+{cc}{r}");
        }
        if let Some(r) = n.strip_prefix(&format!("+{cc}")) {
            n = format!("0{r}");
        }
    }
    if let Some(r) = n.strip_prefix('+') {
        n = format!("00{r}");
    }
    if let Some(r) = n.strip_prefix('0') {
        if emergency.iter().any(|e| e == r) {
            n = r.to_string();
        }
    }
    n
}

/// Converts input such as 0176…, +49…, 0049… to E.164 (national numbers with the site's country code).
pub fn to_e164(input: &str) -> Option<String> {
    to_e164_with(input, dialing().0)
}

pub fn to_e164_with(input: &str, cc: &str) -> Option<String> {
    let n = normalise_with(input, cc, &[]);
    let e = if let Some(r) = n.strip_prefix("00") {
        format!("+{r}")
    } else if let Some(r) = n.strip_prefix('0').filter(|_| !cc.is_empty()) {
        format!("+{cc}{r}")
    } else {
        return None;
    };
    let digits = e.len() - 1;
    (e[1..].chars().all(|c| c.is_ascii_digit()) && !e.starts_with("+0") && (7..=15).contains(&digits)).then_some(e)
}

// --- Targets ---------------------------------------------------------------------------

/// Target from a form: kind = none|user|external|hangup, value = extension or number
#[derive(Deserialize, Default)]
pub struct TargetInput {
    pub kind: String,
    pub value: String,
}

/// Creates or updates the target; returns the new targets.id (None = no target).
pub async fn save_target(db: &PgPool, existing: Option<i32>, t: &TargetInput) -> Result<Option<i32>, AppError> {
    let (kind, user_id, ext): (&str, Option<i32>, Option<String>) = match t.kind.as_str() {
        "" | "none" => return Ok(None),
        "user" | "mailbox_user" => {
            let n: i16 = t.value.trim().parse().map_err(|_| AppError::BadRequest("Invalid extension.".into()))?;
            let id: Option<i32> = sqlx::query_scalar("SELECT id FROM users WHERE extension = $1 AND deleted_at IS NULL")
                .bind(n)
                .fetch_optional(db)
                .await
                .map_err(anyhow::Error::from)?;
            (if t.kind == "mailbox_user" { "mailbox_user" } else { "user" }, Some(id.ok_or_else(|| AppError::BadRequest(format!("No person with extension {n}.")))?), None)
        }
        "external" => {
            let e = to_e164(&t.value).ok_or_else(|| AppError::BadRequest("Invalid external number.".into()))?;
            ("external", None, Some(e))
        }
        "hangup" => ("hangup", None, None),
        "ivr" | "announcement" => {
            let n: i32 = t.value.trim().parse().map_err(|_| AppError::BadRequest("Please select an IVR or announcement.".into()))?;
            let table = if t.kind == "ivr" { "ivrs" } else { "sounds" };
            let ok: Option<i32> = sqlx::query_scalar(&format!("SELECT id FROM {table} WHERE id = $1 AND deleted_at IS NULL"))
                .bind(n)
                .fetch_optional(db)
                .await
                .map_err(anyhow::Error::from)?;
            ok.ok_or_else(|| AppError::BadRequest("IVR or announcement not found.".into()))?;
            let kind = t.kind.as_str();
            let (ivr, sound) = if kind == "ivr" { (Some(n), None) } else { (None, Some(n)) };
            // each column assigned once (Postgres rejects "sound_id = NULL, sound_id = $x")
            let sql = match existing {
                Some(_) => "UPDATE targets SET kind = $2::target_kind, user_id = NULL, group_id = NULL, ivr_id = $3, sound_id = $4,
                            external_e164 = NULL, app_id = NULL, owner_user_id = NULL WHERE id = $1 RETURNING id",
                None => "INSERT INTO targets (kind, ivr_id, sound_id) VALUES ($2::target_kind, $3, $4) RETURNING id",
            };
            let id: i32 = sqlx::query_scalar(sql)
                .bind(existing.unwrap_or(0))
                .bind(kind)
                .bind(ivr)
                .bind(sound)
                .fetch_one(db)
                .await
                .map_err(anyhow::Error::from)?;
            return Ok(Some(id));
        }
        "app" => {
            // value = app number; organization apps only (a personal app belongs to its owner, who
            // could change or delete what numbers and rules lead to)
            let n: i16 = t.value.trim().parse().map_err(|_| AppError::BadRequest("Invalid app number.".into()))?;
            let aid: Option<i32> = sqlx::query_scalar("SELECT id FROM apps WHERE number = $1 AND deleted_at IS NULL AND owner_user_id IS NULL")
                .bind(n)
                .fetch_optional(db)
                .await
                .map_err(anyhow::Error::from)?;
            let aid = aid.ok_or_else(|| AppError::BadRequest(format!("No organization app {n}.")))?;
            let sql = match existing {
                Some(_) => "UPDATE targets SET kind = 'app', user_id = NULL, group_id = NULL, ivr_id = NULL, sound_id = NULL,
                            external_e164 = NULL, app_id = $2, owner_user_id = NULL WHERE id = $1 RETURNING id",
                None => "INSERT INTO targets (kind, app_id) VALUES ('app', $2) RETURNING id",
            };
            let id: i32 = sqlx::query_scalar(sql).bind(existing.unwrap_or(0)).bind(aid).fetch_one(db).await.map_err(anyhow::Error::from)?;
            return Ok(Some(id));
        }
        "group" | "mailbox_group" => {
            let n: i16 = t.value.trim().parse().map_err(|_| AppError::BadRequest("Invalid group number.".into()))?;
            let gid: Option<i32> = sqlx::query_scalar("SELECT id FROM groups WHERE number = $1 AND deleted_at IS NULL")
                .bind(n)
                .fetch_optional(db)
                .await
                .map_err(anyhow::Error::from)?;
            let gid = gid.ok_or_else(|| AppError::BadRequest(format!("No group {n}.")))?;
            let kind = if t.kind == "mailbox_group" { "mailbox_group" } else { "group" };
            return save_target_group(db, existing, gid, kind).await;
        }
        _ => return Err(AppError::BadRequest("Unknown target type.".into())),
    };
    let id = match existing {
        Some(id) => sqlx::query_scalar(
            // A person's own external forward stays theirs (checked against their classes at call
            // time) when an admin re-saves it unchanged; a target the admin changes becomes admin-configured.
            "UPDATE targets SET kind = $2::target_kind, user_id = $3, group_id = NULL, ivr_id = NULL, sound_id = NULL,
                    external_e164 = $4, app_id = NULL,
                    owner_user_id = CASE WHEN kind = $2::target_kind AND external_e164 IS NOT DISTINCT FROM $4 THEN owner_user_id END
             WHERE id = $1 RETURNING id",
)
        .bind(id)
        .bind(kind)
        .bind(user_id)
        .bind(ext)
        .fetch_one(db)
        .await,
        None => sqlx::query_scalar("INSERT INTO targets (kind, user_id, external_e164) VALUES ($1::target_kind, $2, $3) RETURNING id")
            .bind(kind)
            .bind(user_id)
            .bind(ext)
            .fetch_one(db)
            .await,
    }
    .map_err(anyhow::Error::from)?;
    Ok(Some(id))
}

async fn save_target_group(db: &PgPool, existing: Option<i32>, gid: i32, kind: &str) -> Result<Option<i32>, AppError> {
    let id = match existing {
        Some(id) => sqlx::query_scalar(
            "UPDATE targets SET kind = $3::target_kind, user_id = NULL, group_id = $2, ivr_id = NULL, sound_id = NULL,
                    external_e164 = NULL, app_id = NULL, owner_user_id = NULL WHERE id = $1 RETURNING id",
)
        .bind(id)
        .bind(gid)
        .bind(kind)
        .fetch_one(db)
        .await,
        None => sqlx::query_scalar("INSERT INTO targets (kind, group_id) VALUES ($2::target_kind, $1) RETURNING id")
            .bind(gid)
            .bind(kind)
            .fetch_one(db)
            .await,
    }
    .map_err(anyhow::Error::from)?;
    Ok(Some(id))
}

// --- Edit users -------------------------------------------------------------

#[derive(FromRow)]
pub struct UserDetail {
    id: i32,
    extension: i16,
    display_name: String,
    email: Option<String>,
    kind: String,
    language: String,
    enabled: bool,
    allow_sip_devices: bool,
    dnd: bool,
    ring_timeout_s: i16,
    pickup_group: Option<String>,
    route_classes: Vec<String>,
    kc_groups: Vec<String>,
    outbound_did_id: Option<i32>,
    web_access: bool,
    web_admin: bool,
    phone_allowed: bool,
    kc_subject: Option<String>,
    local_login: Option<String>,
    fwd_id: Option<i32>,
    fwd_kind: Option<String>,
    fwd_value: Option<String>,
    noans_id: Option<i32>,
    noans_kind: Option<String>,
    noans_value: Option<String>,
}

impl UserDetail {
    fn pickup(&self) -> String {
        self.pickup_group.clone().unwrap_or_default()
    }
    fn has(&self, class: &str) -> bool {
        self.route_classes.iter().any(|c| c == class)
    }

    /// Class granted through a Keycloak group (cannot be switched off per user)
    fn via_group(&self, group: &Option<String>) -> bool {
        group.as_ref().is_some_and(|g| self.kc_groups.contains(g))
    }

    /// phone_allowed is written by the Keycloak sync (linked users with PBX_OIDC_SYNC=keycloak)
    fn phone_by_sync(&self, kc_sync: &bool) -> bool {
        *kc_sync && self.kc_subject.is_some()
    }

    fn login_name(&self) -> String {
        self.local_login.clone().unwrap_or_default()
    }
}

#[derive(FromRow)]
pub struct DidOption {
    id: i32,
    e164: String,
    label: String,
}

#[derive(Template)]
#[template(path = "admin_user_edit.html")]
struct UserEditPage {
    c: Ctx,
    u: UserDetail,
    dids: Vec<DidOption>,
    /// the outbound number can never be shown (routetest::NEVER_SHOWN)
    out_warning: Option<String>,
    classes: Vec<RouteClass>,
    opts: TargetOptions,
    fwd: TargetSel,
    noans: TargetSel,
    auth: crate::config::AuthUi,
    group_webui: String,
    group_admin: String,
    group_phone: String,
}

const USER_DETAIL: &str = "SELECT u.id, u.extension, u.display_name, u.email, u.kind::text AS kind, u.language, u.enabled, u.allow_sip_devices, u.dnd,
        u.ring_timeout_s, u.pickup_group, u.route_classes, u.kc_groups, u.outbound_did_id,
        u.web_access, u.web_admin, u.phone_allowed, u.kc_subject, (SELECT login FROM local_logins WHERE user_id = u.id) AS local_login,
        f.id AS fwd_id, f.kind::text AS fwd_kind,
        COALESCE(fu.extension::text, (SELECT number::text FROM groups WHERE id = f.group_id), f.ivr_id::text, f.sound_id::text, f.external_e164) AS fwd_value,
        n.id AS noans_id, n.kind::text AS noans_kind,
        COALESCE(nu.extension::text, ng.number::text, n.ivr_id::text, n.sound_id::text, n.external_e164) AS noans_value
    FROM users u
    LEFT JOIN targets f ON f.id = u.forward_immediate LEFT JOIN users fu ON fu.id = f.user_id
    LEFT JOIN targets n ON n.id = u.no_answer_target LEFT JOIN users nu ON nu.id = n.user_id LEFT JOIN groups ng ON ng.id = n.group_id
    WHERE u.id = $1 AND u.deleted_at IS NULL";

pub async fn user_edit(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let u: UserDetail = sqlx::query_as(USER_DETAIL)
        .bind(id)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or(AppError::NotFound)?;
    let dids = sqlx::query_as("SELECT id, e164, label FROM dids ORDER BY e164")
        .fetch_all(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let classes = route_classes(&app.db).await?;
    let fwd = TargetSel::new("fwd_", u.fwd_kind.clone(), u.fwd_value.clone(), "off");
    let noans = TargetSel::new("noans_", u.noans_kind.clone(), u.noans_value.clone(), "“Not available” announcement");
    let c = Ctx::new(&app, &a).section("users");
    let out_warning = crate::routetest::outbound_number(&app.db, id)
        .await?
        .filter(|o| o.never_shown)
        .map(|o| crate::routetest::never_shown_text(&o, &c.t));
    render(UserEditPage {
        c,
        u,
        dids,
        out_warning,
        classes,
        opts: TargetOptions::load(&app.db).await?,
        fwd,
        noans,
        auth: app.cfg.auth_ui(),
        group_webui: app.cfg.group_webui.clone(),
        group_admin: app.cfg.group_admin.clone(),
        group_phone: app.cfg.group_phone.clone(),
    })
}

#[derive(Deserialize)]
pub struct UserForm {
    display_name: String,
    email: String,
    extension: i16,
    #[serde(default)]
    enabled: Option<String>,
    #[serde(default)]
    allow_sip_devices: Option<String>,
    #[serde(default)]
    dnd: Option<String>,
    #[serde(default)]
    web_access: Option<String>,
    #[serde(default)]
    web_admin: Option<String>,
    #[serde(default)]
    phone_allowed: Option<String>,
    ring_timeout_s: i16,
    #[serde(default)]
    pickup_group: String,
    language: String,
    #[serde(default)]
    classes: Vec<String>,
    outbound_did_id: String,
    fwd_kind: String,
    #[serde(default)]
    fwd_value: String,
    noans_kind: String,
    #[serde(default)]
    noans_value: String,
}

/// Places where one of the targets selected by `targets` (a query over `targets` returning ids) is
/// used: callers would fail after deleting what the targets point to. Forwards of person $1 are left
/// out (they are cleared with the person; bind 0 when no person is deleted).
pub fn target_refs(targets: &str) -> String {
    format!(
        "WITH t AS ({targets})
    SELECT 'number ' || d.e164 FROM inbound_rules r JOIN dids d ON d.id = r.did_id
     WHERE r.target_open IN (SELECT id FROM t) OR r.target_closed IN (SELECT id FROM t) OR r.target_holiday IN (SELECT id FROM t)
    UNION ALL SELECT 'IVR ' || i.name FROM ivr_options o JOIN ivrs i ON i.id = o.ivr_id
     WHERE i.deleted_at IS NULL AND o.target_id IN (SELECT id FROM t)
    UNION ALL SELECT 'IVR ' || name || ' (timeout)' FROM ivrs WHERE deleted_at IS NULL AND timeout_target IN (SELECT id FROM t)
    UNION ALL SELECT 'group ' || number || ' (no answer)' FROM groups WHERE deleted_at IS NULL AND no_answer_target IN (SELECT id FROM t)
    UNION ALL SELECT 'forwarding of ' || extension || ' ' || display_name FROM users
     WHERE id <> $1 AND deleted_at IS NULL AND (forward_immediate IN (SELECT id FROM t) OR no_answer_target IN (SELECT id FROM t))
    UNION ALL SELECT 'trunk rule ' || tr.name || ' ' || r.match_prefix FROM trunk_inbound_rules r JOIN trunks tr ON tr.id = r.trunk_id
     WHERE tr.deleted_at IS NULL AND r.target_id IN (SELECT id FROM t)"
)
}

/// Places where user $1 is a call target.
fn user_target_refs() -> String {
    target_refs("SELECT id FROM targets WHERE user_id = $1")
}

/// Places where a personal app of user $1 is a call target (the apps are deleted with the user).
fn user_app_target_refs() -> String {
    target_refs(
        "SELECT t.id FROM targets t JOIN apps a ON a.id = t.app_id WHERE a.owner_user_id = $1 AND a.deleted_at IS NULL",
)
}

/// Deletes a user (soft): devices, personal apps, memberships, contacts, app passwords, voicemails,
/// the personal blocklist and personal call notes go with it; the extension becomes free. Refused
/// while the user or one of their apps is a call target somewhere, and with PBX_PROVISION=auto while
/// the account is still in the phone group (the sync would create the person again).
pub async fn user_delete(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let mut tx = app.db.begin().await.map_err(anyhow::Error::from)?;
    let (extension, name, sub, kc_groups): (i16, String, Option<String>, Vec<String>) =
        sqlx::query_as("SELECT extension, display_name, kc_subject, kc_groups FROM users WHERE id = $1 AND deleted_at IS NULL FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?
            .ok_or(AppError::NotFound)?;
    crate::provision::refuse_if_idp_managed(&app, sub.as_deref(), &kc_groups, "deleted")?;
    let refs: Vec<String> = sqlx::query_scalar(&user_target_refs()).bind(id).fetch_all(&mut *tx).await.map_err(anyhow::Error::from)?;
    if !refs.is_empty() {
        return Err(AppError::BadRequest(format!("{extension} {name} is still a call target: {}. Change these first.", refs.join(", "))));
    }
    let refs: Vec<String> = sqlx::query_scalar(&user_app_target_refs()).bind(id).fetch_all(&mut *tx).await.map_err(anyhow::Error::from)?;
    if !refs.is_empty() {
        return Err(AppError::BadRequest(format!(
            "An app of {extension} {name} is still a call target: {}. Change these first.",
            refs.join(", ")
)));
    }
    let vm_files: Vec<String> =
        sqlx::query_scalar("SELECT file_path FROM voicemails WHERE owner_type = 'user' AND owner_id = $1 AND deleted_at IS NULL")
            .bind(id)
            .fetch_all(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
    let devices: Vec<String> =
        sqlx::query_scalar("UPDATE devices SET deleted_at = now(), enabled = false WHERE user_id = $1 AND deleted_at IS NULL RETURNING sip_username")
            .bind(id)
            .fetch_all(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
    for q in [
        "UPDATE apps SET deleted_at = now(), enabled = false WHERE owner_user_id = $1 AND deleted_at IS NULL",
        "DELETE FROM group_members WHERE user_id = $1",
        // memberships in other people's app call groups (app_group would still ring the person)
        "DELETE FROM app_members WHERE user_id = $1",
        "DELETE FROM contacts WHERE user_id = $1",
        "DELETE FROM dav_tokens WHERE user_id = $1",
        "DELETE FROM local_logins WHERE user_id = $1",
        "DELETE FROM moh_playlists WHERE owner_user_id = $1",
        "DELETE FROM user_blocked_numbers WHERE user_id = $1",
        // shared call notes stay for the colleagues (author shown with the old name)
        "DELETE FROM call_notes WHERE user_id = $1 AND NOT shared",
        "UPDATE voicemails SET deleted_at = now() WHERE owner_type = 'user' AND owner_id = $1 AND deleted_at IS NULL",
        "UPDATE users SET deleted_at = now(), enabled = false, kc_subject = NULL, forward_immediate = NULL, no_answer_target = NULL,
                updated_at = now() WHERE id = $1",
    ] {
        sqlx::query(q).bind(id).execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    }
    tx.commit().await.map_err(anyhow::Error::from)?;
    // the person's messages are gone with them: remove the audio files too
    for f in &vm_files {
        crate::voicemail::remove_audio(f).await;
    }
    // drop the cached endpoints and hang up calls still running on the devices
    for d in &devices {
        app.asterisk_kick(d).await;
    }
    db::audit(
        &app.db,
        &a.session.sub,
        "user.delete",
        &format!("user-id:{id}"),
        serde_json::json!({"extension": extension, "name": name, "kc_subject": sub, "devices": devices}),
)
    .await?;
    Ok(Redirect::to("/admin/users").into_response())
}

pub async fn user_save(
    State(app): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<i32>,
    MultiForm(f): MultiForm<UserForm>,
) -> Result<Response, AppError> {
    let old: UserDetail = sqlx::query_as(USER_DETAIL)
        .bind(id)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or(AppError::NotFound)?;
    if f.extension != old.extension {
        crate::numbers::check(&app.db, &app.cfg.numbers, "person", &f.extension.to_string(), Some(id)).await?;
    }
    if !(5..=300).contains(&f.ring_timeout_s) {
        return Err(AppError::BadRequest("Ring time: 5–300 s.".into()));
    }
    let name = f.display_name.trim();
    if name.is_empty() || name.chars().count() > 40 {
        return Err(AppError::BadRequest("Name: 1–40 characters.".into()));
    }
    // Only accept known, non-built-in classes; internal is always allowed, emergency never needed.
    let known: Vec<String> = sqlx::query_scalar("SELECT key FROM route_classes WHERE NOT builtin")
        .fetch_all(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let mut classes = vec!["internal".to_string()];
    classes.extend(f.classes.iter().filter(|c| known.contains(c)).cloned());
    let did: Option<i32> = f.outbound_did_id.parse().ok();
    let fwd = save_target(&app.db, old.fwd_id, &TargetInput { kind: f.fwd_kind.clone(), value: f.fwd_value.clone() }).await?;
    let noans = save_target(&app.db, old.noans_id, &TargetInput { kind: f.noans_kind.clone(), value: f.noans_value.clone() }).await?;
    if fwd.is_some() && f.fwd_kind == "user" && f.fwd_value.trim() == f.extension.to_string() {
        return Err(AppError::BadRequest("Forwarding to oneself is not allowed.".into()));
    }
    // Login switches; phone permission only where the Keycloak sync does not own it
    let (web_access, web_admin) = (f.web_access.is_some(), f.web_admin.is_some());
    let phone = if old.phone_by_sync(&app.cfg.kc_sync) { old.phone_allowed } else { f.phone_allowed.is_some() };
    if a.session.user_id == Some(id)
        && (f.enabled.is_none() || !crate::auth::permissions(&app.cfg, &a.session.groups, web_access, web_admin).0)
    {
        return Err(AppError::BadRequest("You cannot lock yourself out (admin permission or active switch).".into()));
    }

    let mut tx = app.db.begin().await.map_err(anyhow::Error::from)?;
    sqlx::query(
        "UPDATE users SET display_name = $2, email = $3, extension = $4, enabled = $5, dnd = $6, ring_timeout_s = $7,
                route_classes = $8, outbound_did_id = $9, forward_immediate = $10, no_answer_target = $11, language = $12,
                allow_sip_devices = $13, pickup_group = $14, web_access = $15, web_admin = $16, phone_allowed = $17,
                number_changed_at = CASE WHEN extension <> $4 THEN now() ELSE number_changed_at END,
                updated_at = now()
         WHERE id = $1",
)
    .bind(id)
    .bind(name)
    .bind(Some(f.email.trim()).filter(|e| !e.is_empty()))
    .bind(f.extension)
    .bind(f.enabled.is_some())
    .bind(f.dnd.is_some())
    .bind(f.ring_timeout_s)
    .bind(&classes)
    .bind(did)
    .bind(fwd)
    .bind(noans)
    .bind(if f.language == "de" { "de" } else { "en" })
    .bind(f.allow_sip_devices.is_some())
    .bind({
        let g: String = f.pickup_group.trim().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(30).collect();
        if g.is_empty() { None } else { Some(g) }
    })
    .bind(web_access)
    .bind(web_admin)
    .bind(phone)
    .execute(&mut *tx)
    .await
    .map_err(crate::numbers::db_error)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    db::audit(
        &app.db,
        &a.session.sub,
        "user.edit",
        &format!("user:{}", f.extension),
        serde_json::json!({"old_extension": old.extension, "classes": classes, "enabled": f.enabled.is_some(),
                           "web_access": web_access, "web_admin": web_admin, "phone_allowed": phone}),
)
    .await?;
    if f.extension != old.extension {
        crate::voicemail::mailbox_moved(&app, old.extension).await;
    }
    // name/extension/rights are part of the cached endpoint: refresh, and hang up if the user was
    // disabled or lost the phone permission
    let was_active = old.enabled && old.phone_allowed;
    for d in db::devices_of(&app.db, id).await? {
        if was_active && !(f.enabled.is_some() && phone) {
            app.asterisk_kick(&d.sip_username).await;
        } else {
            app.asterisk_refresh(&d.sip_username).await;
        }
    }
    Ok(Redirect::to(&format!("/admin/users/{id}")).into_response())
}

// --- Permission classes ----------------------------------------------------------------

#[derive(FromRow)]
pub struct RouteClass {
    key: String,
    name: String,
    description: String,
    default_on: bool,
    builtin: bool,
    kc_group: Option<String>,
    routes: i64,
    users: i64,
}

async fn route_classes(db: &PgPool) -> Result<Vec<RouteClass>, AppError> {
    Ok(sqlx::query_as(
        "SELECT c.key, c.name, c.description, c.default_on, c.builtin, c.kc_group,
                (SELECT count(*) FROM outbound_routes r WHERE r.route_class = c.key) AS routes,
                (SELECT count(*) FROM users u WHERE c.key = ANY (u.route_classes) AND u.deleted_at IS NULL) AS users
         FROM route_classes c ORDER BY c.builtin DESC, c.name",
)
    .fetch_all(db)
    .await
    .map_err(anyhow::Error::from)?)
}

#[derive(FromRow)]
pub struct RouteRow {
    id: i32,
    priority: i16,
    name: String,
    match_prefix: String,
    min_len: i16,
    max_len: Option<i16>,
    action: String,
    route_class: String,
    /// trunk names in the order they are tried ("A → B")
    trunks: Option<String>,
}

#[derive(Template)]
#[template(path = "admin_classes.html")]
struct ClassesPage {
    c: Ctx,
    classes: Vec<RouteClass>,
    routes: Vec<RouteRow>,
    test_number: String,
    test_caller: String,
    test: Option<crate::routetest::TestResult>,
}

#[derive(Deserialize, Default)]
pub struct RouteTestQuery {
    #[serde(default)]
    test: String,
    #[serde(default)]
    caller: String,
}

pub async fn classes(
    State(app): State<AppState>,
    Admin(a): Admin,
    axum::extract::Query(q): axum::extract::Query<RouteTestQuery>,
) -> Result<Response, AppError> {
    let test = if q.test.trim().is_empty() {
        None
    } else {
        Some(crate::routetest::evaluate(&app.db, &q.test, &q.caller).await?)
    };
    let routes = sqlx::query_as(
        "SELECT r.id, r.priority, r.name, r.match_prefix, r.min_len, r.max_len, r.action::text AS action, r.route_class,
                (SELECT string_agg(t.name, ' → ' ORDER BY rt.position) FROM route_trunks rt JOIN trunks t ON t.id = rt.trunk_id
                 WHERE rt.route_id = r.id) AS trunks
         FROM outbound_routes r ORDER BY r.priority, r.match_prefix",
)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    render(ClassesPage {
        c: Ctx::new(&app, &a).section("classes"),
        classes: route_classes(&app.db).await?,
        routes,
        test_number: q.test,
        test_caller: q.caller,
        test,
    })
}

#[derive(Deserialize)]
pub struct ClassForm {
    key: String,
    /// "1" when editing an existing class (Edit button); adding refuses a key that exists
    #[serde(default)]
    edit: String,
    name: String,
    description: String,
    #[serde(default)]
    default_on: Option<String>,
    #[serde(default)]
    kc_group: String,
}

pub async fn class_save(State(app): State<AppState>, Admin(a): Admin, Form(f): Form<ClassForm>) -> Result<Response, AppError> {
    let key = f.key.trim().to_lowercase();
    let editing = !f.edit.trim().is_empty();
    let n = sqlx::query(if editing {
        "UPDATE route_classes SET name = $2, description = $3, default_on = $4, kc_group = $5 WHERE key = $1"
    } else {
        "INSERT INTO route_classes (key, name, description, default_on, kc_group) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (key) DO NOTHING"
    })
    .bind(&key)
    .bind(f.name.trim())
    .bind(f.description.trim())
    .bind(f.default_on.is_some())
    .bind(Some(f.kc_group.trim()).filter(|g| !g.is_empty()))
    .execute(&app.db)
    .await
    .map_err(|e| AppError::BadRequest(format!("Saving failed (key: a–z, 0–9, _): {e}")))?
    .rows_affected();
    if n == 0 {
        return Err(AppError::BadRequest(if editing {
            format!("The class {key} no longer exists. Please reload the page.")
        } else {
            format!("The class {key} exists already; use its Edit button.")
        }));
    }
    db::audit(&app.db, &a.session.sub, "class.save", &format!("class:{key}"), serde_json::json!({})).await?;
    Ok(Redirect::to("/admin/classes").into_response())
}

pub async fn class_delete(State(app): State<AppState>, Admin(a): Admin, Path(key): Path<String>) -> Result<Response, AppError> {
    let mut tx = app.db.begin().await.map_err(anyhow::Error::from)?;
    let n = sqlx::query(
        "DELETE FROM route_classes c WHERE key = $1 AND NOT builtin
           AND NOT EXISTS (SELECT 1 FROM outbound_routes r WHERE r.route_class = c.key)",
)
    .bind(&key)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?
    .rows_affected();
    if n == 0 {
        return Err(AppError::BadRequest("Only non-built-in classes without routes can be deleted.".into()));
    }
    sqlx::query("UPDATE users SET route_classes = array_remove(route_classes, $1)")
        .bind(&key)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "class.delete", &format!("class:{key}"), serde_json::json!({})).await?;
    Ok(Redirect::to("/admin/classes").into_response())
}

// --- Numbers ---------------------------------------------------------------------

#[derive(FromRow)]
pub struct DidRow {
    id: i32,
    e164: String,
    label: String,
    trunk: String,
    target_desc: Option<String>,
}

#[derive(FromRow)]
pub struct TrunkOption {
    id: i32,
    name: String,
}

#[derive(Template)]
#[template(path = "admin_numbers.html")]
struct NumbersPage {
    c: Ctx,
    dids: Vec<DidView>,
    trunks: Vec<TrunkOption>,
}

pub struct DidView {
    row: DidRow,
}

pub async fn numbers(State(app): State<AppState>, Admin(a): Admin) -> Result<Response, AppError> {
    let dids: Vec<DidRow> = sqlx::query_as(&format!(
        "SELECT d.id, d.e164, d.label, tr.name AS trunk, {TARGET_DESC} AS target_desc
         FROM dids d JOIN trunks tr ON tr.id = d.trunk_id
         LEFT JOIN inbound_rules ir ON ir.did_id = d.id
         LEFT JOIN targets t ON t.id = ir.target_open {TARGET_JOINS}
         ORDER BY d.e164"
))
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let trunks = sqlx::query_as("SELECT id, name FROM trunks ORDER BY name")
        .fetch_all(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let dids = dids.into_iter().map(|row: DidRow| DidView { row }).collect();
    render(NumbersPage { c: Ctx::new(&app, &a).section("numbers"), dids, trunks })
}

pub async fn number_target(
    State(app): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<i32>,
    Form(t): Form<TargetInput>,
) -> Result<Response, AppError> {
    let existing: Option<i32> = sqlx::query_scalar("SELECT target_open FROM inbound_rules WHERE did_id = $1")
        .bind(id)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    match save_target(&app.db, existing, &t).await? {
        Some(tid) => {
            sqlx::query(
                "INSERT INTO inbound_rules (did_id, target_open) VALUES ($1, $2)
                 ON CONFLICT (did_id) DO UPDATE SET target_open = EXCLUDED.target_open",
)
            .bind(id)
            .bind(tid)
            .execute(&app.db)
            .await
            .map_err(anyhow::Error::from)?;
        }
        None => {
            sqlx::query("DELETE FROM inbound_rules WHERE did_id = $1").bind(id).execute(&app.db).await.map_err(anyhow::Error::from)?;
        }
    }
    db::audit(&app.db, &a.session.sub, "did.target", &format!("did-id:{id}"), serde_json::json!({"kind": t.kind, "value": t.value})).await?;
    Ok(Redirect::to("/admin/numbers").into_response())
}

#[derive(Deserialize)]
pub struct NewDid {
    e164: String,
    trunk_id: i32,
    label: String,
}

pub async fn number_create(State(app): State<AppState>, Admin(a): Admin, Form(f): Form<NewDid>) -> Result<Response, AppError> {
    let e = to_e164(&f.e164).ok_or_else(|| AppError::BadRequest("Invalid phone number.".into()))?;
    sqlx::query("INSERT INTO dids (trunk_id, e164, label) VALUES ($1, $2, $3)")
        .bind(f.trunk_id)
        .bind(&e)
        .bind(f.label.trim())
        .execute(&app.db)
        .await
        .map_err(|e| AppError::BadRequest(format!("Creating failed: {e}")))?;
    db::audit(&app.db, &a.session.sub, "did.add", &e, serde_json::json!({})).await?;
    Ok(Redirect::to("/admin/numbers").into_response())
}

// --- Groups -------------------------------------------------------------------------

#[derive(FromRow)]
pub struct GroupRow {
    id: i32,
    number: i16,
    name: String,
    strategy: String,
    member_timeout_s: i16,
    total_timeout_s: i16,
    moh: bool,
    members: i64,
    noans_kind: Option<String>,
    noans_value: Option<String>,
    noans_desc: Option<String>,
    noans_id: Option<i32>,
    announce_sound_id: Option<i32>,
    moh_playlist_id: Option<i32>,
    /// prompts of the group mailbox ("de"/"en")
    language: String,
    /// shown in front of the caller name on the members' phones (e.g. "DSUP")
    caller_name_prefix: String,
    /// queue mode: callers wait in line (app_queue), position announced every queue_announce_s
    queue: bool,
    queue_announce_s: i16,
}

const GROUP_SELECT: &str = concat!(
    "SELECT g.id, g.number, g.name, g.strategy, g.member_timeout_s, g.total_timeout_s, g.moh,
        (SELECT count(*) FROM group_members m WHERE m.group_id = g.id) AS members,
        t.kind::text AS noans_kind, ",
    target_value!(),
    " AS noans_value, ",
    target_desc!(),
    " AS noans_desc,
        t.id AS noans_id, g.announce_sound_id, g.moh_playlist_id, g.language, g.caller_name_prefix, g.queue, g.queue_announce_s
    FROM groups g LEFT JOIN targets t ON t.id = g.no_answer_target ",
    target_joins!(),
    " WHERE g.deleted_at IS NULL"
);

#[derive(FromRow)]
pub struct MemberRow {
    user_id: i32,
    extension: i16,
    display_name: String,
    enabled: bool,
    logged_in: bool,
}

#[derive(Template)]
#[template(path = "admin_groups.html")]
struct GroupsPage {
    c: Ctx,
    groups: Vec<GroupRow>,
}

#[derive(Template)]
#[template(path = "admin_group.html")]
struct GroupPage {
    c: Ctx,
    g: GroupRow,
    members: Vec<MemberRow>,
    opts: TargetOptions,
    noans: TargetSel,
    playlists: Vec<crate::moh::PlaylistRow>,
}

impl GroupRow {
    fn moh_is(&self, id: &i32) -> bool {
        self.moh_playlist_id == Some(*id)
    }
    fn strategy_label(&self) -> &'static str {
        match self.strategy.as_str() {
            "linear" => "one after another",
            "rrmemory" => "round robin",
            _ => "ring all",
        }
    }

    fn announce_is(&self, id: &str) -> bool {
        self.announce_sound_id.map(|a| a.to_string()) == Some(id.to_string())
    }
}

pub async fn groups(State(app): State<AppState>, Admin(a): Admin) -> Result<Response, AppError> {
    let groups = sqlx::query_as(&format!("{GROUP_SELECT} ORDER BY g.number"))
        .fetch_all(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    render(GroupsPage { c: Ctx::new(&app, &a).section("groups"), groups })
}

#[derive(Deserialize)]
pub struct GroupForm {
    /// empty on create = next free number
    #[serde(default)]
    number: String,
    name: String,
    strategy: String,
    member_timeout_s: i16,
    total_timeout_s: i16,
    #[serde(default)]
    moh: Option<String>,
    #[serde(default)]
    noans_kind: String,
    #[serde(default)]
    noans_value: String,
    #[serde(default)]
    announce: String,
    #[serde(default)]
    moh_playlist: String,
    #[serde(default)]
    language: String,
    #[serde(default)]
    caller_name_prefix: String,
    #[serde(default)]
    queue: Option<String>,
    #[serde(default = "default_announce")]
    queue_announce_s: i16,
}

fn default_announce() -> i16 {
    30
}

/// Caller name prefix: at most 8 characters, none that would break the dialplan's comma-separated
/// ODBC result or the SIP display name (same rule as the CHECK constraint in the schema).
pub fn valid_caller_prefix(p: &str) -> bool {
    p.chars().count() <= 8 && !p.chars().any(|c| c.is_control() || ",;:\"\\|<>".contains(c))
}

fn check_group(f: &GroupForm) -> Result<(), AppError> {
    if !["ringall", "linear", "rrmemory"].contains(&f.strategy.as_str()) {
        return Err(AppError::BadRequest("Unknown strategy.".into()));
    }
    if !(5..=120).contains(&f.member_timeout_s) || !(5..=600).contains(&f.total_timeout_s) {
        return Err(AppError::BadRequest("Timeouts: 5–120 s per member, 5–600 s total.".into()));
    }
    let n = f.name.trim();
    if n.is_empty() || n.chars().count() > 40 {
        return Err(AppError::BadRequest("Name: 1–40 characters.".into()));
    }
    if f.queue_announce_s != 0 && !(15..=300).contains(&f.queue_announce_s) {
        return Err(AppError::BadRequest("Position announcement: every 15–300 s, or 0 = off.".into()));
    }
    if !valid_caller_prefix(f.caller_name_prefix.trim()) {
        return Err(AppError::BadRequest("Caller name prefix: at most 8 characters, no , ; : \" \\ | < >.".into()));
    }
    Ok(())
}

pub async fn group_create(State(app): State<AppState>, Admin(a): Admin, Form(f): Form<GroupForm>) -> Result<Response, AppError> {
    check_group(&f)?;
    let number = crate::numbers::from_form(&app.db, &app.cfg.numbers, "group", &f.number, None).await?;
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO groups (number, name, strategy, member_timeout_s, total_timeout_s, moh)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
)
    .bind(number)
    .bind(f.name.trim())
    .bind(&f.strategy)
    .bind(f.member_timeout_s)
    .bind(f.total_timeout_s)
    .bind(f.moh.is_some())
    .fetch_one(&app.db)
    .await
    .map_err(crate::numbers::db_error)?;
    db::audit(&app.db, &a.session.sub, "group.add", &format!("group:{number}"), serde_json::json!({})).await?;
    Ok(Redirect::to(&format!("/admin/groups/{id}")).into_response())
}

async fn load_group(db: &PgPool, id: i32) -> Result<GroupRow, AppError> {
    sqlx::query_as(&format!("{GROUP_SELECT} AND g.id = $1"))
        .bind(id)
        .fetch_optional(db)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or(AppError::NotFound)
}

pub async fn group(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let g = load_group(&app.db, id).await?;
    let members = sqlx::query_as(
        "SELECT u.id AS user_id, u.extension, u.display_name, m.enabled, m.logged_in
         FROM group_members m JOIN users u ON u.id = m.user_id
         WHERE m.group_id = $1 AND u.deleted_at IS NULL ORDER BY m.priority, u.extension",
)
    .bind(id)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let noans = TargetSel::new("noans_", g.noans_kind.clone(), g.noans_value.clone(), "“Not available” announcement");
    let playlists = crate::moh::company_playlists(&app.db).await?.into_iter().filter(|p| !p.is_default).collect();
    render(GroupPage { c: Ctx::new(&app, &a).section("groups"), g, members, opts: TargetOptions::load(&app.db).await?, noans, playlists })
}

pub async fn group_save(
    State(app): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<i32>,
    Form(f): Form<GroupForm>,
) -> Result<Response, AppError> {
    check_group(&f)?;
    let old = load_group(&app.db, id).await?;
    let number = if f.number.trim() == old.number.to_string() {
        old.number
    } else {
        crate::numbers::check(&app.db, &app.cfg.numbers, "group", &f.number, Some(id)).await?
    };
    if f.noans_kind == "group" && f.noans_value.trim() == number.to_string() {
        return Err(AppError::BadRequest("A group cannot forward to itself.".into()));
    }
    let noans = save_target(&app.db, old.noans_id, &TargetInput { kind: f.noans_kind.clone(), value: f.noans_value.clone() }).await?;
    sqlx::query(
        "UPDATE groups SET number = $2, name = $3, strategy = $4, member_timeout_s = $5, total_timeout_s = $6,
                moh = $7, no_answer_target = $8, announce_sound_id = $9,
                moh_playlist_id = (SELECT id FROM moh_playlists WHERE id = $10 AND owner_user_id IS NULL AND NOT is_default),
                language = $11, caller_name_prefix = $12, queue = $13, queue_announce_s = $14 WHERE id = $1",
)
    .bind(id)
    .bind(number)
    .bind(f.name.trim())
    .bind(&f.strategy)
    .bind(f.member_timeout_s)
    .bind(f.total_timeout_s)
    .bind(f.moh.is_some())
    .bind(noans)
    .bind(f.announce.parse::<i32>().ok())
    .bind(f.moh_playlist.parse::<i32>().ok())
    .bind(if f.language == "en" { "en" } else { "de" })
    .bind(f.caller_name_prefix.trim())
    .bind(f.queue.is_some())
    .bind(f.queue_announce_s)
    .execute(&app.db)
    .await
    .map_err(crate::numbers::db_error)?;
    db::audit(&app.db, &a.session.sub, "group.edit", &format!("group:{number}"), serde_json::json!({"strategy": f.strategy, "caller_name_prefix": f.caller_name_prefix.trim(), "queue": f.queue.is_some()})).await?;
    Ok(Redirect::to(&format!("/admin/groups/{id}")).into_response())
}

#[derive(Deserialize)]
pub struct MemberForm {
    extension: i16,
}

/// Adds a person at the end of the ringing order.
pub async fn group_member_add(
    State(app): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<i32>,
    Form(f): Form<MemberForm>,
) -> Result<Response, AppError> {
    load_group(&app.db, id).await?;
    let n = sqlx::query(
        "INSERT INTO group_members (group_id, user_id, priority)
         SELECT $1, id, LEAST(COALESCE((SELECT max(priority) FROM group_members WHERE group_id = $1), 0) + 10, 32000)
         FROM users WHERE extension = $2 AND deleted_at IS NULL
         ON CONFLICT (group_id, user_id) DO NOTHING",
)
    .bind(id)
    .bind(f.extension)
    .execute(&app.db)
    .await
    .map_err(anyhow::Error::from)?
    .rows_affected();
    if n == 0 {
        let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE extension = $1 AND deleted_at IS NULL)")
            .bind(f.extension)
            .fetch_one(&app.db)
            .await
            .map_err(anyhow::Error::from)?;
        return Err(AppError::BadRequest(if exists {
            format!("{} is already a member.", f.extension)
        } else {
            format!("No person with extension {}.", f.extension)
        }));
    }
    db::audit(&app.db, &a.session.sub, "group.member_add", &format!("group-id:{id}"), serde_json::json!({"extension": f.extension})).await?;
    Ok(Redirect::to(&format!("/admin/groups/{id}")).into_response())
}

#[derive(Deserialize)]
pub struct OrderForm {
    /// user ids in the new ringing order, comma-separated
    order: String,
}

/// New ringing order (drag & drop on the group page): the list must hold exactly the current
/// members, otherwise someone changed them in the meantime and nothing is saved.
pub async fn group_member_order(
    State(app): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<i32>,
    Form(f): Form<OrderForm>,
) -> Result<Response, AppError> {
    load_group(&app.db, id).await?;
    let order: Vec<i32> = f
        .order
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().parse::<i32>())
        .collect::<Result<_, _>>()
        .map_err(|_| AppError::BadRequest("Invalid order.".into()))?;
    let mut tx = app.db.begin().await.map_err(anyhow::Error::from)?;
    let current: Vec<i32> = sqlx::query_scalar(
        "SELECT m.user_id FROM group_members m JOIN users u ON u.id = m.user_id
         WHERE m.group_id = $1 AND u.deleted_at IS NULL FOR UPDATE OF m",
)
    .bind(id)
    .fetch_all(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    let mut want = order.clone();
    want.sort_unstable();
    let mut have = current;
    have.sort_unstable();
    if want != have || order.len() > 3000 {
        return Err(AppError::BadRequest("The member list has changed in the meantime. Please reload the page.".into()));
    }
    for (i, uid) in order.iter().enumerate() {
        sqlx::query("UPDATE group_members SET priority = $3 WHERE group_id = $1 AND user_id = $2")
            .bind(id)
            .bind(uid)
            .bind(((i + 1) * 10) as i16)
            .execute(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
    }
    tx.commit().await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "group.member_order", &format!("group-id:{id}"), serde_json::json!({"order": order})).await?;
    Ok(Redirect::to(&format!("/admin/groups/{id}")).into_response())
}

pub async fn group_member_remove(
    State(app): State<AppState>,
    Admin(a): Admin,
    Path((id, uid)): Path<(i32, i32)>,
) -> Result<Response, AppError> {
    sqlx::query("DELETE FROM group_members WHERE group_id = $1 AND user_id = $2")
        .bind(id)
        .bind(uid)
        .execute(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "group.member_remove", &format!("group-id:{id}"), serde_json::json!({"user_id": uid})).await?;
    Ok(Redirect::to(&format!("/admin/groups/{id}")).into_response())
}

/// Logs a member in or out of a group (admin page; members do it on their start page).
pub async fn set_logged_in(db: &PgPool, actor: &str, gid: i32, uid: i32, on: bool) -> Result<bool, AppError> {
    let n = sqlx::query("UPDATE group_members SET logged_in = $3 WHERE group_id = $1 AND user_id = $2")
        .bind(gid)
        .bind(uid)
        .bind(on)
        .execute(db)
        .await
        .map_err(anyhow::Error::from)?
        .rows_affected();
    if n > 0 {
        db::audit(db, actor, if on { "group.login" } else { "group.logout" }, &format!("group-id:{gid}"), serde_json::json!({"user_id": uid})).await?;
    }
    Ok(n > 0)
}

#[derive(Deserialize)]
pub struct LoginForm {
    #[serde(default)]
    pub on: Option<String>,
}

pub async fn group_member_login(
    State(app): State<AppState>,
    Admin(a): Admin,
    Path((id, uid)): Path<(i32, i32)>,
    Form(f): Form<LoginForm>,
) -> Result<Response, AppError> {
    set_logged_in(&app.db, &a.session.sub, id, uid, f.on.is_some()).await?;
    Ok(Redirect::to(&format!("/admin/groups/{id}")).into_response())
}

pub async fn group_delete(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let used: i64 = sqlx::query_scalar("SELECT count(*) FROM targets WHERE group_id = $1")
        .bind(id)
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    if used > 0 {
        return Err(AppError::BadRequest("The group is still used as a target (number, forwarding or another group).".into()));
    }
    sqlx::query("UPDATE groups SET deleted_at = now() WHERE id = $1").bind(id).execute(&app.db).await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "group.delete", &format!("group-id:{id}"), serde_json::json!({})).await?;
    Ok(Redirect::to("/admin/groups").into_response())
}

// --- Status -----------------------------------------------------------------------------

pub struct Registration {
    name: String,
    status: String,
}

pub struct ContactInfo {
    device: String,
    owner: String,
    address: String,
    user_agent: String,
    status: String,
    rtt_ms: String,
}

pub struct ChannelInfo {
    channel: String,
    caller: String,
    connected: String,
    app: String,
    duration: String,
}

#[derive(Template)]
#[template(path = "admin_status.html")]
struct StatusPage {
    c: Ctx,
    error: Option<String>,
    registrations: Vec<Registration>,
    contacts: Vec<ContactInfo>,
    channels: Vec<ChannelInfo>,
}

fn g(m: &ami::Message, k: &str) -> String {
    m.get(k).cloned().unwrap_or_default()
}

pub async fn status(State(app): State<AppState>, Admin(a): Admin) -> Result<Response, AppError> {
    let mut page = StatusPage {
        c: Ctx::new(&app, &a).section("status"),
        error: None,
        registrations: vec![],
        contacts: vec![],
        channels: vec![],
    };
    let res: anyhow::Result<()> = async {
        for m in ami::list("PJSIPShowRegistrationsOutbound", "OutboundRegistrationDetail").await? {
            page.registrations.push(Registration { name: g(&m, "ObjectName"), status: g(&m, "Status") });
        }
        let owners: Vec<(String, String)> = sqlx::query_as(
            "SELECT d.sip_username, COALESCE(d.number::text || ' · ', '') || u.display_name || ' · ' || d.label
             FROM devices d JOIN users u ON u.id = d.user_id WHERE d.deleted_at IS NULL",
)
        .fetch_all(&app.db)
        .await?;
        for m in ami::list("PJSIPShowContacts", "ContactList").await? {
            let aor = match g(&m, "Endpoint") {
                e if !e.is_empty() => e,
                _ => g(&m, "ObjectName").split(';').next().unwrap_or_default().to_string(),
            };
            let owner = owners.iter().find(|(u, _)| *u == aor).map(|(_, o)| o.clone()).unwrap_or_else(|| "Trunk".into());
            let rtt = g(&m, "RoundtripUsec").parse::<f64>().map(|u| format!("{:.0}", u / 1000.0)).unwrap_or_default();
            page.contacts.push(ContactInfo {
                device: aor,
                owner,
                address: format!("{}:{}", g(&m, "ViaAddr"), g(&m, "ViaPort")),
                user_agent: g(&m, "UserAgent"),
                status: g(&m, "Status"),
                rtt_ms: rtt,
            });
        }
        for m in ami::list("CoreShowChannels", "CoreShowChannel").await? {
            page.channels.push(ChannelInfo {
                channel: g(&m, "Channel"),
                caller: format!("{} {}", g(&m, "CallerIDNum"), g(&m, "CallerIDName")),
                connected: g(&m, "ConnectedLineNum"),
                app: g(&m, "Application"),
                duration: g(&m, "Duration"),
            });
        }
        Ok(())
    }
    .await;
    if let Err(e) = res {
        page.error = Some(format!("{e:#}"));
    }
    render(page)
}

// --- Call log and audit --------------------------------------------------------------

#[derive(FromRow)]
pub struct AuditRow {
    at: String,
    actor: String,
    action: String,
    object: String,
    details: String,
}

#[derive(Template)]
#[template(path = "admin_log.html")]
struct LogPage {
    c: Ctx,
    calls: Vec<crate::callflow::CallLine>,
    audit: Vec<AuditRow>,
}

#[derive(FromRow)]
struct CdrExport {
    calldate: String,
    clid: Option<String>,
    src: Option<String>,
    dst: Option<String>,
    dcontext: Option<String>,
    channel: Option<String>,
    dstchannel: Option<String>,
    lastapp: Option<String>,
    disposition: Option<String>,
    duration: Option<i32>,
    billsec: Option<i32>,
    uniqueid: Option<String>,
    linkedid: Option<String>,
}

#[derive(Deserialize)]
pub struct ExportQuery {
    days: Option<i32>,
}

/// All call records of the last `days` (default 90) as CSV.
pub async fn log_calls_csv(
    State(app): State<AppState>,
    Admin(a): Admin,
    axum::extract::Query(q): axum::extract::Query<ExportQuery>,
) -> Result<Response, AppError> {
    let days = q.days.unwrap_or(90).clamp(1, 3650);
    let rows: Vec<CdrExport> = sqlx::query_as(
        "SELECT to_char(calldate, 'YYYY-MM-DD HH24:MI:SS') AS calldate, clid, src, dst, dcontext,
                channel, dstchannel, lastapp, disposition, duration, billsec, uniqueid, linkedid
         FROM cdr WHERE calldate > now() - make_interval(days => $1) ORDER BY id",
)
    .bind(days)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "cdr.export", "cdr", serde_json::json!({"days": days, "rows": rows.len()})).await?;
    let mut w = crate::csv::Csv::new(&[
        "time", "caller_id", "from", "to", "context", "channel", "dst_channel", "application", "result", "duration_s", "billsec", "uniqueid", "linkedid",
    ]);
    let o = |v: &Option<String>| v.clone().unwrap_or_default();
    for r in &rows {
        w.row(&[
            &r.calldate,
            &o(&r.clid),
            &o(&r.src),
            &o(&r.dst),
            &o(&r.dcontext),
            &o(&r.channel),
            &o(&r.dstchannel),
            &o(&r.lastapp),
            &o(&r.disposition),
            &r.duration.unwrap_or(0).to_string(),
            &r.billsec.unwrap_or(0).to_string(),
            &o(&r.uniqueid),
            &o(&r.linkedid),
        ]);
    }
    Ok(w.into_response("calls"))
}

pub async fn log(State(app): State<AppState>, Admin(a): Admin) -> Result<Response, AppError> {
    let c = Ctx::new(&app, &a).section("log");
    let mut calls = crate::callflow::lines(c.t, &app.db, 500).await?;
    // names of external numbers: shared phone book and external phone books (no private contacts)
    crate::callflow::annotate(&app.db, &app.cfg.country_code, app.cfg.shared_contacts.on(), &mut calls).await?;
    let audit = sqlx::query_as(
        "SELECT to_char(at, 'DD.MM.YY HH24:MI') AS at,
                COALESCE((SELECT display_name FROM users WHERE kc_subject = a.actor OR 'local:' || id = a.actor LIMIT 1), a.actor) AS actor,
                action, object, details::text AS details
         FROM audit_log a ORDER BY id DESC LIMIT 1000",
)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    render(LogPage { c, calls, audit })
}

#[cfg(test)]
mod tests {
    use super::{MAX_ROUTE_TRUNKS, normalise_with, route_trunk_list, to_e164_with, valid_caller_prefix};

    #[test]
    fn route_trunks_from_the_form() {
        let sel = |v: &[&str]| route_trunk_list(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        // form order, empty selects skipped (a gap does not leave a hole in the positions)
        assert_eq!(sel(&["5", "", "4", "", ""]).ok(), Some(vec![5, 4]));
        assert_eq!(sel(&["", "", "", "", ""]).ok(), Some(vec![]));
        assert!(sel(&["5", "4", "5"]).is_err(), "a trunk twice");
        assert!(sel(&["x"]).is_err());
        let many: Vec<String> = (1..=MAX_ROUTE_TRUNKS as i32 + 1).map(|i| i.to_string()).collect();
        assert!(route_trunk_list(&many).is_err());
    }

    #[test]
    fn caller_prefixes() {
        for ok in ["", "DSUP", "ESAL", "Süd-EN", "12345678"] {
            assert!(valid_caller_prefix(ok), "{ok}");
        }
        for bad in ["123456789", "a,b", "a;b", "a:b", "a\"b", "a|b", "<b>", "a\nb"] {
            assert!(!valid_caller_prefix(bad), "{bad}");
        }
    }

    fn em() -> Vec<String> {
        vec!["110".into(), "112".into()]
    }

    #[test]
    fn e164() {
        let to_e164 = |s: &str| to_e164_with(s, "49");
        assert_eq!(to_e164("0176 1234567").as_deref(), Some("+491761234567"));
        assert_eq!(to_e164("+49 40 12345054").as_deref(), Some("+494012345054"));
        assert_eq!(to_e164("0044 20 1234567").as_deref(), Some("+44201234567"));
        assert_eq!(to_e164("1234"), None);
        assert_eq!(to_e164("+0123456789"), None);
        // trunk 0 after the country code
        assert_eq!(to_e164("+49 (0) 40 12345054").as_deref(), Some("+494012345054"));
        assert_eq!(to_e164("0049 040 12345054").as_deref(), Some("+494012345054"));
        assert_eq!(to_e164("112"), None);
        assert_eq!(to_e164("0112"), None);
        // other country; without a country code national numbers cannot be converted
        assert_eq!(to_e164_with("01 5123456", "43").as_deref(), Some("+4315123456"));
        assert_eq!(to_e164_with("+49 40 12345054", "43").as_deref(), Some("+494012345054"));
        assert_eq!(to_e164_with("040 12345054", ""), None);
        assert_eq!(to_e164_with("+49 40 12345054", "").as_deref(), Some("+494012345054"));
    }

    /// Same results as [sub-outbound] in extensions.conf
    #[test]
    fn normalises_like_the_dialplan() {
        let normalise = |s: &str| normalise_with(s, "49", &em());
        assert_eq!(normalise("+49 451 123"), "0451123");
        assert_eq!(normalise("+490451123"), "0451123");
        assert_eq!(normalise("+44 20 1234"), "0044201234");
        assert_eq!(normalise("0049 451"), "0451");
        assert_eq!(normalise("00490451"), "0451");
        assert_eq!(normalise("0044 20"), "004420");
        assert_eq!(normalise("0112"), "112");
        assert_eq!(normalise("0110"), "110");
        assert_eq!(normalise("0115"), "0115");
        assert_eq!(normalise("112"), "112");
        assert_eq!(normalise("116117"), "116117");
        assert_eq!(normalise("0176 123"), "0176123");
        assert_eq!(normalise("781020"), "781020");
        assert_eq!(normalise("+49 112"), "112");
        // idempotent: the dialplan and route_for() may see an already normalised number
        for n in ["+49 451 123", "+44 20", "00490451", "0112", "781020", "0049"] {
            assert_eq!(normalise(&normalise(n)), normalise(n));
        }
        // another site: Austria, emergency 112 only; German numbers are international there
        let at = |s: &str| normalise_with(s, "43", &["112".to_string()]);
        assert_eq!(at("+43 1 5123456"), "015123456");
        assert_eq!(at("+49 40 123"), "004940123");
        assert_eq!(at("0110"), "0110");
        assert_eq!(at("0112"), "112");
    }
}

// --- Outbound routes --------------------------------------------------------------

#[derive(FromRow, Default)]
pub struct RouteEdit {
    id: i32,
    priority: i16,
    name: String,
    match_prefix: String,
    min_len: i16,
    max_len: Option<i16>,
    action: String,
    /// in the order they are tried (route_trunks.position)
    trunk_ids: Vec<i32>,
    strip_digits: i16,
    prepend: String,
    clip_mode: String,
    clip_did_id: Option<i32>,
    route_class: String,
}

/// Trunks one dial rule can list (form: one select per position).
pub const MAX_ROUTE_TRUNKS: usize = 5;

impl RouteEdit {
    /// Is trunk `id` the rule's trunk number `pos` (0-based)?
    fn trunk_at(&self, pos: &usize, id: &i32) -> bool {
        self.trunk_ids.get(*pos) == Some(id)
    }
    fn did_is(&self, id: &i32) -> bool {
        self.clip_did_id == Some(*id)
    }
    fn max_len_str(&self) -> String {
        self.max_len.map(|m| m.to_string()).unwrap_or_default()
    }
}

#[derive(Template)]
#[template(path = "admin_route.html")]
struct RoutePage {
    c: Ctx,
    r: RouteEdit,
    is_new: bool,
    trunks: Vec<TrunkOption>,
    dids: Vec<DidOption>,
    classes: Vec<RouteClass>,
    positions: std::ops::Range<usize>,
}

async fn route_page(app: &AppState, a: &crate::auth::CurrentUser, r: RouteEdit, is_new: bool) -> Result<Response, AppError> {
    let trunks = sqlx::query_as("SELECT id, name FROM trunks ORDER BY name").fetch_all(&app.db).await.map_err(anyhow::Error::from)?;
    let dids = sqlx::query_as("SELECT id, e164, label FROM dids ORDER BY e164").fetch_all(&app.db).await.map_err(anyhow::Error::from)?;
    render(RoutePage {
        c: Ctx::new(app, a).section("classes"),
        r,
        is_new,
        trunks,
        dids,
        classes: route_classes(&app.db).await?,
        positions: 0..MAX_ROUTE_TRUNKS,
    })
}

pub async fn route_new(State(app): State<AppState>, Admin(a): Admin) -> Result<Response, AppError> {
    let r = RouteEdit { priority: 50, min_len: 1, action: "route".into(), clip_mode: "user".into(), route_class: "national".into(), ..Default::default() };
    route_page(&app, &a, r, true).await
}

pub async fn route_edit(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let r: RouteEdit = sqlx::query_as(
        "SELECT id, priority, name, match_prefix, min_len, max_len, action::text AS action,
                ARRAY(SELECT trunk_id FROM route_trunks WHERE route_id = r.id ORDER BY position) AS trunk_ids, strip_digits, prepend,
                clip_mode::text AS clip_mode, clip_did_id, route_class FROM outbound_routes r WHERE id = $1",
)
    .bind(id)
    .fetch_optional(&app.db)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or(AppError::NotFound)?;
    route_page(&app, &a, r, false).await
}

#[derive(Deserialize)]
pub struct RouteForm {
    priority: i16,
    name: String,
    match_prefix: String,
    min_len: i16,
    #[serde(default)]
    max_len: String,
    action: String,
    /// one value per position select, empty = unused
    #[serde(default)]
    trunk_ids: Vec<String>,
    strip_digits: i16,
    #[serde(default)]
    prepend: String,
    clip_mode: String,
    #[serde(default)]
    clip_did_id: String,
    route_class: String,
}

/// (max_len, trunk ids in order, clip_did_id) of a checked route form
type CheckedRoute = (Option<i16>, Vec<i32>, Option<i32>);

/// Validates a route form.
fn check_route(f: &RouteForm) -> Result<CheckedRoute, AppError> {
    let prefix_ok = !f.match_prefix.is_empty()
        && f.match_prefix.len() <= 20
        && f.match_prefix.chars().enumerate().all(|(i, c)| c.is_ascii_digit() || (i == 0 && c == '+'));
    if !prefix_ok {
        return Err(AppError::BadRequest("Prefix: digits only (optionally leading +).".into()));
    }
    if f.name.trim().is_empty() || f.name.chars().count() > 60 {
        return Err(AppError::BadRequest("Name: 1–60 characters.".into()));
    }
    if !(0..=999).contains(&f.priority) || !(1..=30).contains(&f.min_len) || !(0..=20).contains(&f.strip_digits) {
        return Err(AppError::BadRequest("Priority 0–999, min. length 1–30, strip 0–20.".into()));
    }
    if !f.prepend.chars().all(|c| c.is_ascii_digit() || c == '+') || f.prepend.len() > 10 {
        return Err(AppError::BadRequest("Prepend: digits only.".into()));
    }
    let max_len = if f.max_len.trim().is_empty() {
        None
    } else {
        let m: i16 = f.max_len.trim().parse().map_err(|_| AppError::BadRequest("Max. length invalid.".into()))?;
        if m < f.min_len {
            return Err(AppError::BadRequest("Max. length must not be below min. length.".into()));
        }
        Some(m)
    };
    if !["route", "deny"].contains(&f.action.as_str()) || !["user", "route"].contains(&f.clip_mode.as_str()) {
        return Err(AppError::BadRequest("Invalid action or caller ID mode.".into()));
    }
    let trunks = route_trunk_list(&f.trunk_ids)?;
    if f.action == "route" && trunks.is_empty() {
        return Err(AppError::BadRequest("A route with action “route” needs a trunk.".into()));
    }
    Ok((max_len, trunks, f.clip_did_id.parse::<i32>().ok()))
}

/// The selected trunks in form order (empty selects skipped); each trunk at most once.
fn route_trunk_list(sel: &[String]) -> Result<Vec<i32>, AppError> {
    let mut out: Vec<i32> = vec![];
    for v in sel.iter().map(|v| v.trim()).filter(|v| !v.is_empty()) {
        let id: i32 = v.parse().map_err(|_| AppError::BadRequest("Invalid trunk.".into()))?;
        if out.contains(&id) {
            return Err(AppError::BadRequest("Each trunk can be listed only once per dial rule.".into()));
        }
        out.push(id);
    }
    if out.len() > MAX_ROUTE_TRUNKS {
        return Err(AppError::BadRequest(format!("At most {MAX_ROUTE_TRUNKS} trunks per dial rule.")));
    }
    Ok(out)
}

pub async fn route_save(
    State(app): State<AppState>,
    Admin(a): Admin,
    Path(id): Path<String>,
    MultiForm(f): MultiForm<RouteForm>,
) -> Result<Response, AppError> {
    let (max_len, trunks, did) = check_route(&f)?;
    let trunks = if f.action == "route" { trunks } else { vec![] };
    let sql = if id == "new" {
        "INSERT INTO outbound_routes (priority, name, match_prefix, min_len, max_len, action, strip_digits, prepend,
                                      clip_mode, clip_did_id, route_class)
         VALUES ($2, $3, $4, $5, $6, $7::route_action, $8, $9, $10::clip_mode, $11, $12) RETURNING id"
    } else {
        "UPDATE outbound_routes SET priority = $2, name = $3, match_prefix = $4, min_len = $5, max_len = $6,
                action = $7::route_action, strip_digits = $8, prepend = $9, clip_mode = $10::clip_mode,
                clip_did_id = $11, route_class = $12
         WHERE id = $1 RETURNING id"
    };
    let mut tx = app.db.begin().await.map_err(anyhow::Error::from)?;
    let rid: i32 = sqlx::query_scalar(sql)
        .bind(id.parse::<i32>().unwrap_or(0))
        .bind(f.priority)
        .bind(f.name.trim())
        .bind(&f.match_prefix)
        .bind(f.min_len)
        .bind(max_len)
        .bind(&f.action)
        .bind(f.strip_digits)
        .bind(&f.prepend)
        .bind(&f.clip_mode)
        .bind(did)
        .bind(&f.route_class)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| AppError::BadRequest(format!("Saving failed: {e}")))?
        .ok_or(AppError::NotFound)?;
    sqlx::query("DELETE FROM route_trunks WHERE route_id = $1").bind(rid).execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    sqlx::query(
        "INSERT INTO route_trunks (route_id, position, trunk_id)
         SELECT $1, x.pos::smallint, x.trunk_id FROM unnest($2::int[]) WITH ORDINALITY AS x(trunk_id, pos)",
    )
    .bind(rid)
    .bind(&trunks)
    .execute(&mut *tx)
    .await
    .map_err(|e| AppError::BadRequest(format!("Saving failed: {e}")))?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "route.save", &format!("route-id:{rid}"),
        serde_json::json!({"prefix": f.match_prefix, "action": f.action, "class": f.route_class, "trunks": trunks})).await?;
    Ok(Redirect::to("/admin/classes").into_response())
}

pub async fn route_delete(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let n = sqlx::query("DELETE FROM outbound_routes WHERE id = $1 AND route_class <> 'emergency'")
        .bind(id)
        .execute(&app.db)
        .await
        .map_err(anyhow::Error::from)?
        .rows_affected();
    if n == 0 {
        return Err(AppError::BadRequest("Emergency routes cannot be deleted.".into()));
    }
    db::audit(&app.db, &a.session.sub, "route.delete", &format!("route-id:{id}"), serde_json::json!({})).await?;
    Ok(Redirect::to("/admin/classes").into_response())
}

/// POST /admin/routes/new (see trunks::trunk_create)
pub async fn route_create(state: State<AppState>, admin: Admin, form: MultiForm<RouteForm>) -> Result<Response, AppError> {
    route_save(state, admin, Path("new".to_string()), form).await
}
