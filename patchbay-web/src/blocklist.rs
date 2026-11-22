// SPDX-License-Identifier: GPL-3.0-or-later
//! Blocklist for incoming calls (admin page). Entries are E.164 numbers or prefixes ending in '*';
//! the dialplan asks the database (`caller_blocked()`) for every call from a trunk
//! and answers blocked callers with "busy". Anonymous callers can be blocked as a whole.
//!
//! Personal blocklist (start page, call widget): full numbers a person blocks for themselves; only
//! calls to the person personally hear busy (`user_caller_blocked()` in sub-user),
//! calls to their call groups and queues still ring.

use askama::Template;
use axum::{
    Form, Json,
    extract::{Multipart, Path, Query, State},
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use sqlx::FromRow;

use crate::{
    AppError, AppState,
    auth::{Admin, CurrentUser},
    companion::Reply,
    db,
    ui::{Ctx, render},
};

#[derive(FromRow)]
pub struct Entry {
    id: i32,
    pattern: String,
    note: String,
    created_by: String,
    created_at: String,
    hits: i32,
    last_hit: Option<String>,
}

#[derive(Template)]
#[template(path = "admin_blocklist.html")]
struct Page {
    c: Ctx,
    entries: Vec<Entry>,
    block_anonymous: bool,
    /// prefilled from the call log ("Block" link)
    number: String,
    /// result of a CSV import
    msg: String,
}

#[derive(Deserialize, Default)]
pub struct PageQuery {
    #[serde(default)]
    number: String,
    #[serde(default)]
    msg: String,
}

pub async fn page(State(app): State<AppState>, Admin(a): Admin, Query(q): Query<PageQuery>) -> Result<Response, AppError> {
    let entries = sqlx::query_as(
        "SELECT id, pattern, note, created_by, to_char(created_at AT TIME ZONE $1, 'YYYY-MM-DD') AS created_at, hits,
                to_char(last_hit AT TIME ZONE $1, 'YYYY-MM-DD HH24:MI') AS last_hit
         FROM blocked_numbers ORDER BY pattern",
)
    .bind(&app.cfg.timezone)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let block_anonymous: bool = sqlx::query_scalar("SELECT COALESCE((SELECT value = 'true' FROM settings WHERE key = 'block_anonymous'), false)")
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let number: String = q.number.chars().filter(|c| c.is_ascii_digit() || *c == '+').take(20).collect();
    let msg: String = q.msg.chars().filter(|c| !c.is_control()).take(300).collect();
    let page = Page { c: Ctx::new(&app, &a).section("blocklist"), entries, block_anonymous, number, msg };
    render(page)
}

/// "+49 40 123", "040 123…", "0900*", "+44*" → "+4940123…", "+49900*", "+44*"
pub fn pattern(input: &str, cc: &str) -> Option<String> {
    let t = input.trim();
    let (body, prefix) = match t.strip_suffix('*') {
        Some(b) => (b, true),
        None => (t, false),
    };
    if body.chars().any(|c| !(c.is_ascii_digit() || c == '+' || c == ' ' || c == '-' || c == '/' || c == '(' || c == ')')) {
        return None;
    }
    let mut n: String = body.chars().filter(|c| c.is_ascii_digit() || *c == '+').collect();
    if let Some(r) = n.strip_prefix("00") {
        n = format!("+{r}");
    } else if let Some(r) = n.strip_prefix('0').filter(|_| !cc.is_empty()) {
        n = format!("+{cc}{r}");
    }
    if !cc.is_empty() {
        if let Some(r) = n.strip_prefix(&format!("+{cc}0")) {
            n = format!("+{cc}{r}");
        }
    }
    let digits = n.strip_prefix('+')?;
    let ok = !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
        && !digits.starts_with('0')
        && digits.len() <= 15
        // a prefix may be as short as a country code (+44*), a full number needs 6+ digits
        && digits.len() >= if prefix { 2 } else { 6 };
    ok.then(|| if prefix { format!("{n}*") } else { n })
}

#[derive(Deserialize)]
pub struct AddForm {
    number: String,
    #[serde(default)]
    note: String,
}

pub async fn add(State(app): State<AppState>, Admin(a): Admin, Form(f): Form<AddForm>) -> Result<Response, AppError> {
    let p = pattern(&f.number, &app.cfg.country_code)
        .ok_or_else(|| AppError::BadRequest("Number: international (+49…) or national (0…), optionally ending in * for a prefix.".into()))?;
    let note: String = f.note.trim().chars().filter(|c| !c.is_control()).take(200).collect();
    sqlx::query(
        "INSERT INTO blocked_numbers (pattern, note, created_by) VALUES ($1, $2, $3)
         ON CONFLICT (pattern) DO UPDATE SET note = EXCLUDED.note",
)
    .bind(&p)
    .bind(&note)
    .bind(&a.session.sub)
    .execute(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "blocklist.add", &p, serde_json::json!({"note": note})).await?;
    Ok(Redirect::to("/admin/blocklist").into_response())
}

const MAX_CSV: usize = 2 * 1024 * 1024;

/// CSV lines "number[;note]" (separator ; , or tab, quotes allowed). Lines whose first field is no
/// number (headers, comments) are skipped. Returns (pattern, note) pairs and the number of skipped lines.
pub fn parse_csv(text: &str, cc: &str) -> (Vec<(String, String)>, usize) {
    let (mut out, mut skipped) = (vec![], 0);
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            continue;
        }
        let sep = [';', '\t', ','].into_iter().find(|s| line.contains(*s));
        let mut fields = match sep {
            Some(s) => line.splitn(2, s).map(|f| f.trim().trim_matches('"').trim().to_string()).collect::<Vec<_>>(),
            None => vec![line.trim_matches('"').to_string()],
        };
        let note: String = if fields.len() > 1 { fields.remove(1) } else { String::new() };
        let note: String = note.chars().filter(|c| !c.is_control()).take(200).collect();
        match pattern(&fields[0], cc) {
            Some(p) => out.push((p, note)),
            None => skipped += 1,
        }
    }
    (out, skipped)
}

pub async fn import(State(app): State<AppState>, Admin(a): Admin, mut mp: Multipart) -> Result<Response, AppError> {
    let mut text = String::new();
    while let Some(field) = mp.next_field().await.map_err(|e| AppError::BadRequest(e.to_string()))? {
        if field.name() == Some("file") {
            let bytes = field.bytes().await.map_err(|e| AppError::BadRequest(e.to_string()))?;
            if bytes.len() > MAX_CSV {
                return Err(AppError::BadRequest("File too large (max. 2 MB).".into()));
            }
            text = String::from_utf8_lossy(&bytes).into_owned();
        }
    }
    let (entries, skipped) = parse_csv(&text, &app.cfg.country_code);
    let mut added = 0u64;
    let mut tx = app.db.begin().await.map_err(anyhow::Error::from)?;
    for (p, note) in &entries {
        // existing entries keep their note unless the file has one
        added += sqlx::query(
            "INSERT INTO blocked_numbers (pattern, note, created_by) VALUES ($1, $2, $3)
             ON CONFLICT (pattern) DO UPDATE SET note = EXCLUDED.note WHERE EXCLUDED.note <> ''",
)
        .bind(p)
        .bind(note)
        .bind(&a.session.sub)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?
        .rows_affected();
    }
    tx.commit().await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "blocklist.import", "csv", serde_json::json!({"entries": entries.len(), "skipped": skipped})).await?;
    let msg = format!("Import: {} numbers read, {added} added or updated, {skipped} lines skipped.", entries.len());
    let enc: String = msg.bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
    Ok(Redirect::to(&format!("/admin/blocklist?msg={enc}")).into_response())
}

pub async fn delete(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let p: Option<String> = sqlx::query_scalar("DELETE FROM blocked_numbers WHERE id = $1 RETURNING pattern")
        .bind(id)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    if let Some(p) = p {
        db::audit(&app.db, &a.session.sub, "blocklist.delete", &p, serde_json::json!({})).await?;
    }
    Ok(Redirect::to("/admin/blocklist").into_response())
}

#[derive(Deserialize)]
pub struct AnonForm {
    #[serde(default)]
    block_anonymous: Option<String>,
}

pub async fn anonymous(State(app): State<AppState>, Admin(a): Admin, Form(f): Form<AnonForm>) -> Result<Response, AppError> {
    let on = f.block_anonymous.is_some();
    sqlx::query("INSERT INTO settings (key, value) VALUES ('block_anonymous', $1) ON CONFLICT (key) DO UPDATE SET value = $1")
        .bind(if on { "true" } else { "false" })
        .execute(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "blocklist.anonymous", "settings", serde_json::json!({"block": on})).await?;
    Ok(Redirect::to("/admin/blocklist").into_response())
}

// --- Personal blocklist ----------------------------------------------------------------------

/// Entries per person
const MAX_PERSONAL: i64 = 200;

/// A full external number as E.164, no prefix (personal blocklist, call notes): the same rules as
/// the admin list and `caller_e164()` in the database. Internal numbers give None.
pub fn full_number(input: &str, cc: &str) -> Option<String> {
    pattern(input, cc).filter(|p| !p.ends_with('*'))
}

#[derive(FromRow)]
pub struct PersonalEntry {
    pub id: i32,
    pub e164: String,
    pub note: String,
    pub created_at: String,
    pub hits: i32,
    pub last_hit: Option<String>,
}

pub async fn personal_list(app: &AppState, uid: i32) -> Result<Vec<PersonalEntry>, AppError> {
    Ok(sqlx::query_as(
        "SELECT id, e164, note, to_char(created_at AT TIME ZONE $2, 'YYYY-MM-DD') AS created_at, hits,
                to_char(last_hit AT TIME ZONE $2, 'YYYY-MM-DD HH24:MI') AS last_hit
         FROM user_blocked_numbers WHERE user_id = $1 ORDER BY e164",
)
    .bind(uid)
    .bind(&app.cfg.timezone)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?)
}

/// Is the number on the person's own blocklist (`e164` from `full_number`)
pub async fn personal_blocked(app: &AppState, uid: i32, e164: &str) -> Result<bool, AppError> {
    Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM user_blocked_numbers WHERE user_id = $1 AND e164 = $2)")
        .bind(uid)
        .bind(e164)
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?)
}

/// Adds a number to the person's own blocklist (an existing entry keeps it and gets the new note).
pub async fn personal_add(app: &AppState, u: &CurrentUser, uid: i32, number: &str, note: &str) -> Result<(), AppError> {
    let e164 = full_number(number, &app.cfg.country_code)
        .ok_or_else(|| AppError::BadRequest("Number: a full external number, international (+49…) or national (0…).".into()))?;
    let note: String = note.trim().chars().filter(|c| !c.is_control()).take(200).collect();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM user_blocked_numbers WHERE user_id = $1 AND e164 <> $2")
        .bind(uid)
        .bind(&e164)
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    if count >= MAX_PERSONAL {
        return Err(AppError::BadRequest("At most 200 blocked numbers. Remove one first.".into()));
    }
    sqlx::query(
        "INSERT INTO user_blocked_numbers (user_id, e164, note) VALUES ($1, $2, $3)
         ON CONFLICT (user_id, e164) DO UPDATE SET note = EXCLUDED.note WHERE EXCLUDED.note <> ''",
)
    .bind(uid)
    .bind(&e164)
    .bind(&note)
    .execute(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    db::audit(&app.db, &u.session.sub, "user.block", &format!("user-id:{uid}"), serde_json::json!({"number": e164, "note": note})).await?;
    Ok(())
}

/// POST /me/blocklist (start page)
pub async fn personal_create(State(app): State<AppState>, u: CurrentUser, Form(f): Form<AddForm>) -> Result<Response, AppError> {
    let uid = crate::companion::uid(&u)?;
    personal_add(&app, &u, uid, &f.number, &f.note).await?;
    Ok(Redirect::to("/#blocklist").into_response())
}

/// POST /me/live/block (number, note): "Block this number for me" in the call widget
pub async fn personal_widget(State(app): State<AppState>, u: CurrentUser, Form(f): Form<AddForm>) -> Result<Json<Reply>, AppError> {
    let uid = crate::companion::uid(&u)?;
    crate::companion::reply(personal_add(&app, &u, uid, &f.number, &f.note).await)
}

/// POST /me/blocklist/{id}/delete (start page)
pub async fn personal_delete(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>) -> Result<Response, AppError> {
    let uid = crate::companion::uid(&u)?;
    let e164: Option<String> = sqlx::query_scalar("DELETE FROM user_blocked_numbers WHERE id = $1 AND user_id = $2 RETURNING e164")
        .bind(id)
        .bind(uid)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    if let Some(e164) = e164 {
        db::audit(&app.db, &u.session.sub, "user.unblock", &format!("user-id:{uid}"), serde_json::json!({"number": e164})).await?;
    }
    Ok(Redirect::to("/#blocklist").into_response())
}

#[cfg(test)]
mod tests {
    use super::pattern;

    #[test]
    fn patterns() {
        assert_eq!(pattern("040 123456", "49").as_deref(), Some("+4940123456"));
        assert_eq!(pattern("+49 (0)40 123456", "49").as_deref(), Some("+4940123456"));
        assert_eq!(pattern("0049 40 123456", "49").as_deref(), Some("+4940123456"));
        assert_eq!(pattern("0900*", "49").as_deref(), Some("+49900*"));
        assert_eq!(pattern("+44*", "49").as_deref(), Some("+44*"));
        assert_eq!(pattern("+4*", "49"), None);
        assert_eq!(pattern("1020", "49"), None);
        assert_eq!(pattern("040 12a", "49"), None);
        assert_eq!(pattern("*", "49"), None);
        assert_eq!(pattern("", "49"), None);
    }

    #[test]
    fn full_numbers_only() {
        assert_eq!(super::full_number("040 123456", "49").as_deref(), Some("+4940123456"));
        assert_eq!(super::full_number("+49 (0)40 123456", "49").as_deref(), Some("+4940123456"));
        assert_eq!(super::full_number("0900*", "49"), None, "no prefixes");
        assert_eq!(super::full_number("1020", "49"), None, "internal");
        assert_eq!(super::full_number("", "49"), None);
    }

    #[test]
    fn csv() {
        let (e, skipped) = super::parse_csv("\u{feff}Nummer;Notiz\n040 123456;\"Spam, Gewinnspiel\"\n+44*\n\n0900*,teuer\nkeine nummer\n", "49");
        assert_eq!(skipped, 2);
        assert_eq!(e, vec![
            ("+4940123456".to_string(), "Spam, Gewinnspiel".to_string()),
            ("+44*".to_string(), String::new()),
            ("+49900*".to_string(), "teuer".to_string()),
        ]);
    }
}
