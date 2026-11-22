// SPDX-License-Identifier: GPL-3.0-or-later
//! Click-to-call, ring test and provisioning status (called from the web UI via fetch).

use axum::{
    Form, Json,
    extract::{Path, State},
};
use serde::{Deserialize, Serialize};

use crate::{AppError, AppState, ami, auth::CurrentUser, db};

#[derive(Serialize)]
pub struct Reply {
    ok: bool,
    message: String,
}

fn reply(ok: bool, message: impl Into<String>) -> Json<Reply> {
    Json(Reply { ok, message: message.into() })
}

/// Finds a device the user may operate (own device, or any device for admins).
async fn device_for(app: &AppState, u: &CurrentUser, id: i32) -> Result<(String, i16), AppError> {
    let row: Option<(String, i16, i32)> = sqlx::query_as(
        "SELECT a.sip_username, a.extension, a.user_id FROM active_devices a WHERE a.id = $1",
    )
    .bind(id)
    .fetch_optional(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let (sip, ext, owner) = row.ok_or(AppError::NotFound)?;
    if !u.is_admin && u.session.user_id != Some(owner) {
        return Err(AppError::NotFound);
    }
    Ok((sip, ext))
}

pub async fn ring_test(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>) -> Result<Json<Reply>, AppError> {
    let (sip, ext) = device_for(&app, &u, id).await?;
    if !ami::is_registered(&sip).await.unwrap_or(false) {
        return Ok(reply(false, "not registered"));
    }
    ami::originate(&format!("PJSIP/{sip}"), "ring-test", "s", &format!("\"Ring test\" <{ext}>"), &[]).await?;
    db::audit(&app.db, &u.session.sub, "device.ring_test", &format!("device-id:{id}"), serde_json::json!({})).await?;
    Ok(reply(true, "ringing"))
}

#[derive(Serialize)]
pub struct ProvStatus {
    fetched: bool,
    registered: bool,
}

/// Polled by the QR code page: has Linphone fetched the configuration, has it registered?
pub async fn prov_status(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>) -> Result<Json<ProvStatus>, AppError> {
    let (sip, _) = device_for(&app, &u, id).await?;
    let fetched: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM provisioning_tokens WHERE device_id = $1 AND kind = 'bootstrap'
                          AND used_at IS NOT NULL AND used_at > now() - interval '30 minutes')",
    )
    .bind(id)
    .fetch_one(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let registered = fetched && ami::is_registered(&sip).await.unwrap_or(false);
    Ok(Json(ProvStatus { fetched, registered }))
}

#[derive(Deserialize)]
pub struct CallForm {
    to: String,
    /// device id, or empty/"all" for all of the user's devices
    #[serde(default)]
    device: String,
}

/// Click-to-call: rings the user's devices, then dials `to` with the user's permissions.
pub async fn click_to_call(State(app): State<AppState>, u: CurrentUser, Form(f): Form<CallForm>) -> Result<Json<Reply>, AppError> {
    let uid = u.phone_uid()?;
    Ok(match originate_for(&app, uid, &f.to, f.device.parse::<i32>().ok(), &u.session.sub).await? {
        Ok(()) => reply(true, "ringing"),
        Err(why) => reply(false, why),
    })
}

/// Rings the user's devices (or the given one), then dials `to` through the dialplan with the
/// user's own permissions (context click2call → from-device; route classes are checked there).
/// Used by the web UI and by the chat bot; `actor` goes into the audit log.
pub async fn originate_for(app: &AppState, uid: i32, to: &str, device: Option<i32>, actor: &str) -> anyhow::Result<Result<(), &'static str>> {
    let to: String = to.chars().filter(|c| c.is_ascii_digit() || *c == '+' || *c == '*').collect();
    if to.is_empty() || to.len() > 20 {
        return Ok(Err("invalid number"));
    }
    let (ext, name): (i16, String) = sqlx::query_as("SELECT extension, display_name FROM users WHERE id = $1").bind(uid).fetch_one(&app.db).await?;
    let ext_s = ext.to_string();
    let clean_name: String = name.chars().filter(|c| !matches!(c, '"' | '\r' | '\n' | ',')).collect();
    // Ring the chosen device first, or all devices of the user
    let channel = match device {
        Some(id) => {
            let sip: Option<String> = sqlx::query_scalar("SELECT sip_username FROM active_devices WHERE id = $1 AND user_id = $2")
                .bind(id)
                .bind(uid)
                .fetch_optional(&app.db)
                .await?;
            let Some(sip) = sip else { return Ok(Err("unknown device")) };
            if !ami::is_registered(&sip).await.unwrap_or(false) {
                return Ok(Err("not registered"));
            }
            format!("PJSIP/{sip}")
        }
        None => format!("Local/{ext}@group-member"),
    };
    ami::originate(&channel, "click2call", &to, &format!("\"→ {to}\" <{to}>"), &[("PBX_C2C_EXT", &ext_s), ("PBX_C2C_NAME", &clean_name)]).await?;
    db::audit(&app.db, actor, "call.click_to_call", &format!("user:{ext}"), serde_json::json!({"to": to})).await?;
    Ok(Ok(()))
}

// --- Call page ---------------------------------------------------------------------------------

use axum::{
    extract::Query,
    response::{IntoResponse, Response},
};

#[derive(Deserialize)]
pub struct CallQuery {
    #[serde(default)]
    to: String,
}

/// GET /call?to=…: the call page is gone (dialling is in the companion widget); old links and
/// bookmarks open the start page with the number in the widget.
pub async fn call_page(Query(q): Query<CallQuery>) -> Response {
    let to: String = q.to.chars().filter(|c| c.is_ascii_digit() || *c == '+' || *c == '*').take(20).collect();
    axum::response::Redirect::to(&if to.is_empty() { "/".to_string() } else { format!("/?dial={}", to.replace('+', "%2B")) }).into_response()
}
