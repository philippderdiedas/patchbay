// SPDX-License-Identifier: GPL-3.0-or-later
//! Persons without an admin (PBX_PROVISION):
//! - `auto` (company): every OIDC user in the phone group gets a person with the next free number,
//!   at login and in the Keycloak sync (also people who never log in).
//! - `self` (GURU style, e.g. a makerspace): users claim a number on the start page; phone
//!   permission does not need the phone group.
//!
//! The person is linked to the OIDC subject (Keycloak: the user id), like an admin would link it.

use axum::{
    Form,
    extract::State,
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::PrivateCookieJar;
use serde::Deserialize;

use crate::{AppError, AppState, auth::CurrentUser, config::Provision, db};

/// Name for CALLERID and the phone book: no quotes or control characters, at most 40 characters.
fn clean_name(name: &str, fallback: &str) -> String {
    let n: String = name.chars().filter(|c| !c.is_control() && !matches!(c, '"' | '<' | '>' | '\\')).take(40).collect();
    let n = n.trim().to_string();
    if n.is_empty() { fallback.chars().take(40).collect() } else { n }
}

/// Creates a person linked to `sub` with phone permission. `number`: chosen number (checked
/// against the person range) or None for the next free one (`random`: a random free one).
pub async fn create_person(
    app: &AppState,
    actor: &str,
    sub: &str,
    name: &str,
    email: Option<&str>,
    number: Option<&str>,
    random: bool,
    groups: &[String],
) -> Result<i32, AppError> {
    let extension = match number.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => crate::numbers::check_self(&app.db, &app.cfg.numbers, "person", n, None).await?,
        None => crate::numbers::allocate_with(&app.db, &app.cfg.numbers, "person", random)
            .await?
            .ok_or_else(|| AppError::BadRequest("Numbers for persons are switched off.".into()))?,
    };
    let name = clean_name(name, &format!("{extension}"));
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO users (kind, extension, display_name, email, kc_subject, phone_allowed, kc_groups, route_classes)
         VALUES ('person', $1, $2, $3, $4, true, $5, ARRAY(SELECT key FROM route_classes WHERE default_on ORDER BY key))
         RETURNING id",
    )
    .bind(extension)
    .bind(&name)
    .bind(email)
    .bind(sub)
    // the groups of the login: every request checks web access/admin against the stored groups
    .bind(groups)
    .fetch_one(&app.db)
    .await
    .map_err(crate::numbers::db_error)?;
    db::audit(&app.db, actor, "user.provision", &format!("user:{extension}"), serde_json::json!({"name": name, "mode": format!("{:?}", app.cfg.provision)}))
        .await?;
    tracing::info!("provisioned person {extension} ({name}) for {sub}");
    Ok(id)
}

/// Login (`auth::callback`) of an OIDC user without a person: with PBX_PROVISION=auto and the phone
/// group, the person is created right away.
pub async fn at_login(app: &AppState, sub: &str, name: &str, email: Option<&str>, groups: &[String]) -> Result<Option<i32>, AppError> {
    if app.cfg.provision != Provision::Auto || !groups.contains(&app.cfg.group_phone) {
        return Ok(None);
    }
    Ok(Some(create_person(app, "system:provision", sub, name, email, None, false, groups).await?))
}

/// PBX_PROVISION=auto with the Keycloak sync: a person whose account is (per the groups the last
/// sync or login stored) in the phone group would be created again by the next sync or login when
/// deleted or unlinked here. Removal belongs to Keycloak; refuse with what to do instead. `what`:
/// "deleted" or "unlinked". Without the sync the stored groups are only as fresh as the last login,
/// so nothing is refused (deleting could lock out the admin for good).
pub fn refuse_if_idp_managed(app: &AppState, sub: Option<&str>, kc_groups: &[String], what: &str) -> Result<(), AppError> {
    if app.cfg.provision != Provision::Auto || !app.cfg.kc_sync || sub.is_none() || !kc_groups.contains(&app.cfg.group_phone) {
        return Ok(());
    }
    Err(AppError::BadRequest(format!(
        "This person is created automatically for the Keycloak group {g} (PBX_PROVISION=auto) and cannot be {what} \
         here: the next sync or login would create it again. Remove the account from group {g} in Keycloak; the next \
         Keycloak sync (every 10 minutes, or “Sync with Keycloak now” under Users) then revokes the phone access and logs out its devices. The \
         person is kept with its number, locked, and can be {what} here afterwards.",
        g = app.cfg.group_phone
    )))
}

#[derive(Deserialize)]
pub struct ClaimForm {
    #[serde(default)]
    number: String,
    #[serde(default)]
    random: Option<String>,
}

/// POST /me/claim (PBX_PROVISION=self): the logged-in OIDC user takes a number; the session is
/// renewed with the new person.
pub async fn claim(State(app): State<AppState>, jar: PrivateCookieJar, u: CurrentUser, Form(f): Form<ClaimForm>) -> Result<Response, AppError> {
    if app.cfg.provision != Provision::SelfService {
        return Err(AppError::NotFound);
    }
    if u.session.user_id.is_some() {
        return Ok(Redirect::to("/").into_response());
    }
    if u.session.is_local() || u.session.sub.starts_with("automation:") {
        return Err(AppError::Forbidden("Only accounts of the single sign-on can claim a number.".into()));
    }
    let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE kc_subject = $1 AND deleted_at IS NULL)")
        .bind(&u.session.sub)
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    if taken {
        return Err(AppError::BadRequest("Your account already has a number – please log in again.".into()));
    }
    let number = if f.random.is_some() { None } else { Some(f.number.as_str()) };
    let uid = create_person(&app, &u.session.sub, &u.session.sub, &u.session.name, u.session.email.as_deref(), number, f.random.is_some(), &u.session.groups)
        .await?;
    let mut session = u.session.clone();
    session.user_id = Some(uid);
    let jar = crate::auth::start_session(jar, &session, u.lang)?;
    Ok((jar, Redirect::to("/")).into_response())
}

/// Keycloak sync with PBX_PROVISION=auto: persons for members of the phone group that have none yet.
/// `members`: (sub, username, e-mail, display name) of enabled accounts in the group.
pub async fn sync_members(app: &AppState, members: &[(String, String, Option<String>, String)]) -> anyhow::Result<usize> {
    let linked: Vec<String> = sqlx::query_scalar("SELECT kc_subject FROM users WHERE kc_subject IS NOT NULL AND deleted_at IS NULL").fetch_all(&app.db).await?;
    let mut created = 0;
    for (sub, username, email, name) in members {
        if linked.contains(sub) {
            continue;
        }
        let name = if name.is_empty() { username } else { name };
        // groups: the same sync run reads them from Keycloak right afterwards
        match create_person(app, "system:keycloak-sync", sub, name, email.as_deref(), None, false, &[]).await {
            Ok(_) => created += 1,
            Err(e) => tracing::warn!("provisioning {username} failed: {e:?}"),
        }
    }
    Ok(created)
}

/// GET /admin/provision/preview: members of the phone group (Keycloak) and whether they already
/// have a person, i.e. what PBX_PROVISION=auto would create. JSON for admins.
pub async fn preview(State(app): State<AppState>, crate::auth::Admin(_a): crate::auth::Admin) -> Result<axum::Json<serde_json::Value>, AppError> {
    let token = crate::sync::token(&app).await?;
    let members = crate::sync::group_members(&app, &token, &app.cfg.group_phone).await?;
    let linked: Vec<(String, i16)> = sqlx::query_as("SELECT kc_subject, extension FROM users WHERE kc_subject IS NOT NULL AND deleted_at IS NULL")
        .fetch_all(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let rows: Vec<serde_json::Value> = members
        .iter()
        .map(|(sub, username, _, name)| {
            let ext = linked.iter().find(|(s, _)| s == sub).map(|(_, e)| *e);
            serde_json::json!({"username": username, "name": name, "extension": ext})
        })
        .collect();
    Ok(axum::Json(serde_json::json!({"group": app.cfg.group_phone, "members": rows})))
}

#[cfg(test)]
mod tests {
    use super::clean_name;

    #[test]
    fn names() {
        assert_eq!(clean_name("  Ada \"Lovelace\" <x>", "1000"), "Ada Lovelace x");
        assert_eq!(clean_name("\u{7}", "1000"), "1000");
    }
}
