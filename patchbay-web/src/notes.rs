// SPDX-License-Identifier: GPL-3.0-or-later
//! Call notes per number (call widget, docs/architecture.md): shown on calls from and to
//! the number. Personal by default; the author may mark a note "visible to all colleagues" while
//! the site allows it (PBX_SHARED_NOTES). With the switch off no new shared notes are made and
//! existing ones are only visible to their author. Only the author edits or deletes a note.
//! Numbers are E.164 (`blocklist::full_number`): internal numbers get no notes.

use axum::{
    Form, Json,
    extract::{Path, State},
};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::{
    AppError, AppState,
    auth::CurrentUser,
    companion::{Reply, reply, uid},
    db,
};

/// Characters per note
const MAX_BODY: usize = 1000;
/// Notes per person
const MAX_PER_USER: i64 = 500;

#[derive(Serialize, FromRow)]
pub struct Note {
    pub id: i32,
    pub body: String,
    /// visible to all colleagues (always false while PBX_SHARED_NOTES is off)
    pub shared: bool,
    /// written by the viewer: may edit and delete
    pub mine: bool,
    pub author: String,
    pub at: String,
}

/// Notes on a number the person may see: own ones and, while the site allows it, shared ones.
pub async fn notes_for(app: &AppState, uid: i32, e164: &str) -> Result<Vec<Note>, AppError> {
    Ok(sqlx::query_as(
        "SELECT n.id, n.body, n.shared AND $3 AS shared, n.user_id = $1 AS mine, u.display_name AS author,
                to_char(n.updated_at AT TIME ZONE $4, 'DD.MM.YY HH24:MI') AS at
         FROM call_notes n JOIN users u ON u.id = n.user_id
         WHERE n.e164 = $2 AND (n.user_id = $1 OR (n.shared AND $3))
         ORDER BY n.updated_at DESC LIMIT 20",
    )
    .bind(uid)
    .bind(e164)
    .bind(app.cfg.shared_notes)
    .bind(&app.cfg.timezone)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?)
}

/// Notes on the numbers of a contact (contact page): own ones and, while the site allows it, shared ones.
pub async fn notes_for_numbers(app: &AppState, uid: i32, numbers: &[String]) -> Result<Vec<(String, Note)>, AppError> {
    let rows: Vec<NumberNote> = sqlx::query_as(
        "SELECT n.e164, n.id, n.body, n.shared AND $3 AS shared, n.user_id = $1 AS mine, u.display_name AS author,
                to_char(n.updated_at AT TIME ZONE $4, 'DD.MM.YY HH24:MI') AS at
         FROM call_notes n JOIN users u ON u.id = n.user_id
         WHERE n.e164 = ANY($2) AND (n.user_id = $1 OR (n.shared AND $3))
         ORDER BY n.updated_at DESC LIMIT 100",
    )
    .bind(uid)
    .bind(numbers)
    .bind(app.cfg.shared_notes)
    .bind(&app.cfg.timezone)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(rows.into_iter().map(|r| (r.e164, r.note)).collect())
}

#[derive(FromRow)]
struct NumberNote {
    e164: String,
    #[sqlx(flatten)]
    note: Note,
}

/// Note text: trimmed, no control characters except line breaks, 1–1000 characters.
fn clean_body(s: &str) -> Option<String> {
    let b: String = s.trim().replace("\r\n", "\n").chars().filter(|c| *c == '\n' || !c.is_control()).collect();
    (!b.is_empty() && b.chars().count() <= MAX_BODY).then_some(b)
}

#[derive(Deserialize)]
pub struct NoteForm {
    /// new notes: the number (raw or E.164)
    #[serde(default)]
    pub(crate) number: String,
    pub(crate) body: String,
    /// "1": visible to all colleagues
    #[serde(default)]
    pub(crate) shared: String,
}

/// Creates (id None) or changes an own note.
pub(crate) async fn save(app: &AppState, u: &CurrentUser, uid: i32, id: Option<i32>, f: &NoteForm) -> Result<(), AppError> {
    let body = clean_body(&f.body).ok_or_else(|| AppError::BadRequest("Note: 1 to 1000 characters.".into()))?;
    let shared = app.cfg.shared_notes && f.shared == "1";
    let (nid, e164): (i32, String) = match id {
        None => {
            let e164 = crate::blocklist::full_number(&f.number, &app.cfg.country_code)
                .ok_or_else(|| AppError::BadRequest("Notes only for external numbers.".into()))?;
            let count: i64 =
                sqlx::query_scalar("SELECT count(*) FROM call_notes WHERE user_id = $1").bind(uid).fetch_one(&app.db).await.map_err(anyhow::Error::from)?;
            if count >= MAX_PER_USER {
                return Err(AppError::BadRequest("At most 500 notes. Delete old ones first.".into()));
            }
            sqlx::query_as("INSERT INTO call_notes (user_id, e164, body, shared) VALUES ($1, $2, $3, $4) RETURNING id, e164")
                .bind(uid)
                .bind(&e164)
                .bind(&body)
                .bind(shared)
                .fetch_one(&app.db)
                .await
                .map_err(anyhow::Error::from)?
        }
        // with shared notes switched off the flag stays as it was (hidden, not cleared)
        Some(id) => sqlx::query_as(
            "UPDATE call_notes SET body = $3, shared = CASE WHEN $5 THEN $4 ELSE shared END, updated_at = now()
             WHERE id = $1 AND user_id = $2 RETURNING id, e164",
        )
        .bind(id)
        .bind(uid)
        .bind(&body)
        .bind(shared)
        .bind(app.cfg.shared_notes)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or(AppError::NotFound)?,
    };
    let action = if id.is_none() { "note.create" } else { "note.update" };
    db::audit(&app.db, &u.session.sub, action, &format!("note:{nid}"), serde_json::json!({"number": e164, "shared": shared})).await?;
    Ok(())
}

/// POST /me/live/notes (number, body, shared)
pub async fn create(State(app): State<AppState>, u: CurrentUser, Form(f): Form<NoteForm>) -> Result<Json<Reply>, AppError> {
    let uid = uid(&u)?;
    reply(save(&app, &u, uid, None, &f).await)
}

/// POST /me/live/notes/{id} (body, shared): own notes only
pub async fn update(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>, Form(f): Form<NoteForm>) -> Result<Json<Reply>, AppError> {
    let uid = uid(&u)?;
    reply(save(&app, &u, uid, Some(id), &f).await)
}

/// POST /me/live/notes/{id}/delete: own notes only
pub async fn delete(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>) -> Result<Json<Reply>, AppError> {
    let uid = uid(&u)?;
    let e164: Option<String> = sqlx::query_scalar("DELETE FROM call_notes WHERE id = $1 AND user_id = $2 RETURNING e164")
        .bind(id)
        .bind(uid)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let Some(e164) = e164 else { return Err(AppError::NotFound) };
    db::audit(&app.db, &u.session.sub, "note.delete", &format!("note:{id}"), serde_json::json!({"number": e164})).await?;
    reply(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::clean_body;

    #[test]
    fn note_text() {
        assert_eq!(clean_body("  Rückruf wegen Angebot\r\nPreis 12 €  ").as_deref(), Some("Rückruf wegen Angebot\nPreis 12 €"));
        assert_eq!(clean_body("a\u{7}b").as_deref(), Some("ab"), "control characters go");
        assert_eq!(clean_body("   "), None);
        assert!(clean_body(&"ä".repeat(1000)).is_some());
        assert_eq!(clean_body(&"ä".repeat(1001)), None);
    }
}
