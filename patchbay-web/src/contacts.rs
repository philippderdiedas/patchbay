// SPDX-License-Identifier: GPL-3.0-or-later
//! Self-service: personal contacts and the user's own call list ("My calls", from the CDR).

use askama::Template;
use axum::{
    Form,
    extract::{Path, State},
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use sqlx::FromRow;

use crate::{AppError, AppState, admin::to_e164, auth::CurrentUser, ui::{Ctx, render}};

// --- Contacts ------------------------------------------------------------------------------

#[derive(FromRow, Clone, Default)]
pub struct Contact {
    pub id: i32,
    pub name: String,
    pub organization: Option<String>,
    pub phone: Option<String>,
    pub mobile: Option<String>,
    pub email: Option<String>,
    pub note: Option<String>,
    /// call notes on the contact's numbers the viewer can see (phone book list)
    #[sqlx(default)]
    pub notes: i64,
}

impl Contact {
    pub fn org(&self) -> String {
        self.organization.clone().unwrap_or_default()
    }
    pub fn phone_str(&self) -> String {
        self.phone.clone().unwrap_or_default()
    }
    pub fn mobile_str(&self) -> String {
        self.mobile.clone().unwrap_or_default()
    }
    pub fn email_str(&self) -> String {
        self.email.clone().unwrap_or_default()
    }
    pub fn note_str(&self) -> String {
        self.note.clone().unwrap_or_default()
    }
}

pub async fn contacts_of(db: &sqlx::PgPool, uid: i32, shared_notes: bool, may_phone: bool) -> Result<Vec<Contact>, AppError> {
    Ok(sqlx::query_as(
        "SELECT c.id, c.name, c.organization, c.phone, c.mobile, c.email, c.note,
                CASE WHEN $3 THEN (SELECT count(*) FROM call_notes n WHERE n.e164 IN (c.phone_e164, c.mobile_e164) AND (n.user_id = $1 OR (n.shared AND $2))) ELSE 0 END AS notes
         FROM contacts c WHERE c.user_id = $1 ORDER BY lower(c.name)",
    )
        .bind(uid)
        .bind(shared_notes)
        .bind(may_phone)
        .fetch_all(db)
        .await
        .map_err(anyhow::Error::from)?)
}

#[derive(Template)]
#[template(path = "contact.html")]
struct ContactPage {
    c: Ctx,
    k: Contact,
    is_new: bool,
    /// call notes on the contact's numbers: (number, note)
    notes: Vec<(String, crate::notes::Note)>,
    /// the contact's external numbers (E.164) for a new note
    note_numbers: Vec<String>,
    shared_notes: bool,
}

#[derive(Deserialize, Default)]
pub struct ContactPrefill {
    #[serde(default)]
    phone: String,
    #[serde(default)]
    name: String,
}

/// New contact; `?phone=…&name=…` prefill it (call companion "Save as contact").
pub async fn contact_new(State(app): State<AppState>, u: CurrentUser, axum::extract::Query(p): axum::extract::Query<ContactPrefill>) -> Result<Response, AppError> {
    u.phone_uid()?;
    let phone: String = p.phone.chars().filter(|c| c.is_ascii_digit() || *c == '+').take(20).collect();
    let name: String = p.name.chars().filter(|c| !c.is_control()).take(80).collect();
    let k = Contact { phone: Some(phone).filter(|s| !s.is_empty()), name, ..Default::default() };
    render(ContactPage { c: Ctx::new(&app, &u), k, is_new: true, notes: vec![], note_numbers: vec![], shared_notes: false })
}

pub async fn contact_edit(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let k: Contact = sqlx::query_as("SELECT id, name, organization, phone, mobile, email, note FROM contacts WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(uid)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or(AppError::NotFound)?;
    // call notes only with phone permission (like the call window)
    let note_numbers = if u.may_phone { contact_numbers(&app.db, uid, id).await? } else { vec![] };
    let notes = crate::notes::notes_for_numbers(&app, uid, &note_numbers).await?;
    render(ContactPage { c: Ctx::new(&app, &u), k, is_new: false, notes, note_numbers, shared_notes: app.cfg.shared_notes })
}

/// External numbers (E.164) of an own contact.
async fn contact_numbers(db: &sqlx::PgPool, uid: i32, id: i32) -> Result<Vec<String>, AppError> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as("SELECT phone_e164, mobile_e164 FROM contacts WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(uid)
        .fetch_optional(db)
        .await
        .map_err(anyhow::Error::from)?;
    let (p, m) = row.ok_or(AppError::NotFound)?;
    let mut v: Vec<String> = [p, m].into_iter().flatten().collect();
    v.dedup();
    Ok(v)
}

#[derive(Deserialize)]
pub struct ContactNoteForm {
    number: String,
    body: String,
    #[serde(default)]
    shared: String,
}

/// POST /contacts/{id}/notes: new call note on one of the contact's numbers.
pub async fn contact_note_create(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>, Form(f): Form<ContactNoteForm>) -> Result<Response, AppError> {
    // same permission as the call window's notes (phone permission, not only a person)
    let uid = crate::companion::uid(&u)?;
    if !contact_numbers(&app.db, uid, id).await?.contains(&f.number) {
        return Err(AppError::BadRequest("Notes only for the contact's external numbers.".into()));
    }
    let form = crate::notes::NoteForm { number: f.number, body: f.body, shared: f.shared };
    crate::notes::save(&app, &u, uid, None, &form).await?;
    Ok(Redirect::to(&format!("/contacts/{id}#notes")).into_response())
}

/// POST /contacts/{id}/notes/{note}/delete: own notes only.
pub async fn contact_note_delete(State(app): State<AppState>, u: CurrentUser, Path((id, note)): Path<(i32, i32)>) -> Result<Response, AppError> {
    let uid = crate::companion::uid(&u)?;
    let e164: Option<String> = sqlx::query_scalar("DELETE FROM call_notes WHERE id = $1 AND user_id = $2 RETURNING e164")
        .bind(note)
        .bind(uid)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let Some(e164) = e164 else { return Err(AppError::NotFound) };
    crate::db::audit(&app.db, &u.session.sub, "note.delete", &format!("note:{note}"), serde_json::json!({"number": e164})).await?;
    Ok(Redirect::to(&format!("/contacts/{id}#notes")).into_response())
}

#[derive(Deserialize)]
pub struct ContactForm {
    name: String,
    #[serde(default)]
    organization: String,
    #[serde(default)]
    phone: String,
    #[serde(default)]
    mobile: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    note: String,
}

fn opt(s: &str, max: usize) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.chars().take(max).collect())
}

/// Phone numbers: external numbers are stored as typed plus E.164 for matching; short internal numbers as digits.
fn number(label: &str, s: &str) -> Result<(Option<String>, Option<String>), AppError> {
    let Some(v) = opt(s, 40) else { return Ok((None, None)) };
    if !v.chars().all(|c| c.is_ascii_digit() || " +-/()".contains(c)) {
        return Err(AppError::BadRequest(format!("{label}: only digits, spaces and + - / ( ).")));
    }
    let digits: String = v.chars().filter(|c| c.is_ascii_digit()).collect();
    if !v.starts_with('+') && !digits.starts_with('0') && (2..=7).contains(&digits.len()) {
        return Ok((Some(digits), None)); // internal number
    }
    let e = to_e164(&v).ok_or_else(|| AppError::BadRequest(format!("{label}: not a valid phone number.")))?;
    Ok((Some(v), Some(e)))
}

pub async fn contact_save(State(app): State<AppState>, u: CurrentUser, Path(id): Path<String>, Form(f): Form<ContactForm>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let name = opt(&f.name, 80).ok_or_else(|| AppError::BadRequest("Name missing.".into()))?;
    let (phone, phone_e164) = number("Phone", &f.phone)?;
    let (mobile, mobile_e164) = number("Mobile", &f.mobile)?;
    // Contacts synced from a phone (CardDAV) keep their stored card; the edit is applied to it.
    let synced = match id.parse::<i32>() {
        Ok(cid) => crate::carddav::contact_card(&app.db, uid, cid).await?.and_then(|(f, v)| v.map(|v| (f, v))),
        Err(_) => None,
    };
    if phone.is_none() && mobile.is_none() && synced.is_none() {
        return Err(AppError::BadRequest("Enter at least one phone number.".into()));
    }
    let new_fields = crate::carddav::Fields {
        name: name.clone(),
        organization: opt(&f.organization, 80),
        phone: phone.clone(),
        mobile: mobile.clone(),
        email: opt(&f.email, 120),
        note: opt(&f.note, 500),
    };
    let vcard = synced.map(|(old, v)| crate::carddav::patch_vcard(&v, &old, &new_fields));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM contacts WHERE user_id = $1").bind(uid).fetch_one(&app.db).await.map_err(anyhow::Error::from)?;
    let q = if id == "new" {
        if count >= crate::carddav::MAX_CONTACTS {
            return Err(AppError::BadRequest("At most 2000 contacts.".into()));
        }
        "INSERT INTO contacts (user_id, name, organization, phone, phone_e164, mobile, mobile_e164, email, note)
         VALUES ($2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING id"
    } else {
        "UPDATE contacts SET name = $3, organization = $4, phone = $5, phone_e164 = $6, mobile = $7, mobile_e164 = $8,
                email = $9, note = $10, vcard = COALESCE($11, vcard), updated_at = now() WHERE id = $1 AND user_id = $2 RETURNING id"
    };
    let id: i32 = if id == "new" { 0 } else { id.parse().map_err(|_| AppError::NotFound)? };
    sqlx::query_scalar::<_, i32>(q)
        .bind(id)
        .bind(uid)
        .bind(&name)
        .bind(opt(&f.organization, 80))
        .bind(&phone)
        .bind(&phone_e164)
        .bind(&mobile)
        .bind(&mobile_e164)
        .bind(opt(&f.email, 120))
        .bind(opt(&f.note, 500))
        .bind(vcard)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or(AppError::NotFound)?;
    Ok(Redirect::to("/phonebook#contacts").into_response())
}

pub async fn contact_create(state: State<AppState>, u: CurrentUser, form: Form<ContactForm>) -> Result<Response, AppError> {
    contact_save(state, u, Path("new".to_string()), form).await
}

pub async fn contact_delete(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    sqlx::query("DELETE FROM contacts WHERE id = $1 AND user_id = $2").bind(id).bind(uid).execute(&app.db).await.map_err(anyhow::Error::from)?;
    Ok(Redirect::to("/phonebook#contacts").into_response())
}

// --- CardDAV app passwords ----------------------------------------------------------------------

#[derive(FromRow)]
pub struct DavToken {
    pub id: i32,
    pub label: String,
    pub created: String,
    pub last_used: Option<String>,
}

/// Login name for CardDAV app passwords (independent of the extension, which may change)
pub fn dav_user(uid: i32) -> String {
    format!("dav{uid}")
}

pub async fn dav_tokens_of(db: &sqlx::PgPool, uid: i32) -> Result<Vec<DavToken>, AppError> {
    Ok(sqlx::query_as(
        "SELECT id, label, to_char(created_at, 'DD.MM.YYYY') AS created,
                to_char(last_used_at, 'DD.MM.YYYY HH24:MI') AS last_used
         FROM dav_tokens WHERE user_id = $1 ORDER BY id",
    )
    .bind(uid)
    .fetch_all(db)
    .await
    .map_err(anyhow::Error::from)?)
}

#[derive(Template)]
#[template(path = "dav_token.html")]
struct DavTokenPage {
    c: Ctx,
    label: String,
    url: String,
    username: String,
    password: String,
}

#[derive(Deserialize)]
pub struct DavTokenForm {
    label: String,
}

/// Creates an app password for CardDAV clients; it is shown once and stored only as HA1.
pub async fn dav_token_create(State(app): State<AppState>, u: CurrentUser, Form(f): Form<DavTokenForm>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    token_page(&app, &u, uid, &f.label).await
}

/// POST /admin/users/{id}/carddav: an admin creates an app password for a person, e.g. a function
/// account without a login (FRITZ!Box online phone book of a shared phone).
pub async fn admin_dav_token_create(
    State(app): State<AppState>,
    crate::auth::Admin(u): crate::auth::Admin,
    Path(uid): Path<i32>,
    Form(f): Form<DavTokenForm>,
) -> Result<Response, AppError> {
    token_page(&app, &u, uid, &f.label).await
}

/// Creates the app password of `uid` and shows it once.
async fn token_page(app: &AppState, u: &CurrentUser, uid: i32, label: &str) -> Result<Response, AppError> {
    let label = opt(label, 60).ok_or_else(|| AppError::BadRequest("Name missing.".into()))?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM dav_tokens WHERE user_id = $1").bind(uid).fetch_one(&app.db).await.map_err(anyhow::Error::from)?;
    if count >= crate::carddav::MAX_TOKENS {
        return Err(AppError::BadRequest("At most 10 app passwords. Revoke one first.".into()));
    }
    let username = dav_user(uid);
    let password = crate::db::random_string(32);
    let ha1 = crate::carddav::md5_hex(&format!("{username}:{}:{password}", app.cfg.sip_realm));
    let id: i32 = sqlx::query_scalar("INSERT INTO dav_tokens (user_id, label, ha1) VALUES ($1, $2, $3) RETURNING id")
        .bind(uid)
        .bind(&label)
        .bind(&ha1)
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    crate::db::audit(&app.db, &u.session.sub, "carddav.token.create", &format!("dav-token:{id}"), serde_json::json!({"user_id": uid, "label": label})).await?;
    let url = format!("{}/carddav/", app.cfg.public_url.trim_end_matches('/'));
    render(DavTokenPage { c: Ctx::new(app, u), label, url, username, password })
}

pub async fn dav_token_delete(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let n = sqlx::query("DELETE FROM dav_tokens WHERE id = $1 AND user_id = $2").bind(id).bind(uid).execute(&app.db).await.map_err(anyhow::Error::from)?;
    if n.rows_affected() > 0 {
        crate::db::audit(&app.db, &u.session.sub, "carddav.token.delete", &format!("dav-token:{id}"), serde_json::json!({"user_id": uid})).await?;
    }
    Ok(Redirect::to("/phonebook#carddav").into_response())
}

// --- My calls -------------------------------------------------------------------------------

#[derive(FromRow)]
pub struct CallRow {
    pub at: String,
    pub at_iso: String,
    pub direction: String,
    pub answered: bool,
    pub number: String,
    pub cid_name: String,
    pub billsec: i32,
    pub name: Option<String>,
}

impl CallRow {
    pub fn duration(&self) -> String {
        if !self.answered {
            return String::new();
        }
        format!("{}:{:02}", self.billsec / 60, self.billsec % 60)
    }
    /// Best name: own contact / colleague, else the caller ID name
    pub fn display_name(&self) -> String {
        self.name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| if self.cid_name == self.number { String::new() } else { self.cid_name.clone() })
    }
}

#[derive(Template)]
#[template(path = "calls.html")]
struct CallsPage {
    c: Ctx,
    calls: Vec<CallRow>,
}

/// Calls of the user's devices: one line per call (CDR rows grouped by linkedid). Outgoing = a device
/// is the calling channel, incoming = a device was dialled (answered by any device or missed).
pub async fn calls_of(db: &sqlx::PgPool, uid: i32, limit: i64) -> Result<Vec<CallRow>, AppError> {
    Ok(sqlx::query_as(
        "WITH dev AS (SELECT 'PJSIP/' || sip_username AS ch FROM devices WHERE user_id = $1),
         legs AS (
           SELECT c.linkedid, c.start, c.disposition, c.billsec, c.src, c.dst, c.clid,
                  CASE WHEN split_part(c.channel, '-', 1) IN (SELECT ch FROM dev) THEN 'out' ELSE 'in' END AS direction
           FROM cdr c
           WHERE c.start > now() - interval '90 days'
             AND c.dcontext NOT IN ('ring-test')
             AND (split_part(c.channel, '-', 1) IN (SELECT ch FROM dev) OR split_part(c.dstchannel, '-', 1) IN (SELECT ch FROM dev))
         ),
         calls AS (
           SELECT linkedid, direction, min(start) AS start, bool_or(disposition = 'ANSWERED') AS answered,
                  max(billsec) FILTER (WHERE disposition = 'ANSWERED') AS billsec,
                  (array_agg(CASE WHEN direction = 'out' THEN dst ELSE src END ORDER BY start))[1] AS number,
                  (array_agg(substring(clid from '^\"(.*)\" <')  ORDER BY start))[1] AS cid_name
           FROM legs GROUP BY linkedid, direction
         )
         SELECT to_char(c.start, 'DD.MM.YY HH24:MI') AS at,
                to_char(c.start, 'YYYY-MM-DD HH24:MI:SS') AS at_iso, c.direction, c.answered,
                COALESCE(c.number, '') AS number,
                CASE WHEN c.direction = 'out' THEN '' ELSE COALESCE(c.cid_name, '') END AS cid_name,
                COALESCE(c.billsec, 0) AS billsec,
                COALESCE(
                  (SELECT k.name FROM contacts k WHERE k.user_id = $1 AND (k.phone = c.number OR k.mobile = c.number
                       OR k.phone_e164 = c.number OR k.mobile_e164 = c.number
                       OR k.phone_e164 = nat.e164 OR k.mobile_e164 = nat.e164) LIMIT 1),
                  (SELECT x.display_name FROM users x WHERE x.extension::text = c.number AND x.deleted_at IS NULL)) AS name
         FROM calls c
         -- national number (0…) as E.164 with the site's country code (settings.country_code, PBX_COUNTRY_CODE)
         LEFT JOIN LATERAL (SELECT '+' || s.value || substr(c.number, 2) AS e164 FROM settings s
                            WHERE s.key = 'country_code' AND c.number LIKE '0%' AND c.number NOT LIKE '00%') nat ON true
         -- drop internal helper legs (empty / invalid / 's' destinations) and calls to oneself through a group
         WHERE COALESCE(c.number, '') NOT IN ('', 'i', 's', 'h')
           AND NOT (c.direction = 'in' AND c.number = (SELECT extension::text FROM users WHERE id = $1))
         ORDER BY c.start DESC LIMIT $2",
    )
    .bind(uid)
    .bind(limit)
    .fetch_all(db)
    .await
    .map_err(anyhow::Error::from)?)
}

/// The user's recent calls for the chat bot ("calls" command).
pub async fn recent_calls(db: &sqlx::PgPool, uid: i32, limit: i64) -> anyhow::Result<Vec<patchbay_notify::pbx::CallListEntry>> {
    let rows = calls_of(db, uid, limit).await.map_err(|e| match e {
        AppError::Internal(e) => e,
        other => anyhow::anyhow!("{other:?}"),
    })?;
    Ok(rows
        .into_iter()
        .map(|k| patchbay_notify::pbx::CallListEntry {
            name: k.display_name(),
            at: k.at,
            incoming: k.direction == "in",
            answered: k.answered,
            number: k.number,
            duration_s: k.billsec,
        })
        .collect())
}

pub async fn calls(State(app): State<AppState>, u: CurrentUser) -> Result<Response, AppError> {
    let calls = calls_of(&app.db, u.phone_uid()?, 1000).await?;
    render(CallsPage { c: Ctx::new(&app, &u), calls })
}

/// "My calls" as CSV (all calls of the last 90 days).
pub async fn calls_csv(State(app): State<AppState>, u: CurrentUser) -> Result<Response, AppError> {
    let calls = calls_of(&app.db, u.phone_uid()?, 100_000).await?;
    let mut w = crate::csv::Csv::new(&["time", "direction", "answered", "number", "name", "duration_s"]);
    for k in &calls {
        w.row(&[&k.at_iso, &k.direction, if k.answered { "yes" } else { "no" }, &k.number, &k.display_name(), &k.billsec.to_string()]);
    }
    Ok(w.into_response("my-calls"))
}
