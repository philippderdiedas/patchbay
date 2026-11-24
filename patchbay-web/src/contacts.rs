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
// Personal contacts (user_id = the person) and, with PBX_SHARED_CONTACTS, the shared phone book
// (user_id NULL): everyone with phone access reads it; everyone (write) or only admins (read) edit it.
// Deleting a shared entry moves it to a bin for BIN_DAYS days (admins restore it, housekeeping purges).

/// Days a deleted shared entry stays in the bin
pub const BIN_DAYS: i32 = 30;
/// Entries in the shared phone book
pub const MAX_SHARED: i64 = 10000;

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
    /// entry of the shared phone book
    #[sqlx(default)]
    pub shared: bool,
    /// shared entries: "Tim · 21.11.26 14:03"
    #[sqlx(default)]
    pub created: Option<String>,
    #[sqlx(default)]
    pub updated: Option<String>,
    /// bin: "Tim · 23.11.26 09:12"
    #[sqlx(default)]
    pub deleted: Option<String>,
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

/// Columns of `Contact` (table alias c); `$tz` is the bind position of the time zone, `$notes` of
/// "count notes" (bool) and `$uid`/`$shn` of the viewer and PBX_SHARED_NOTES for the note count.
fn contact_cols(tz: usize, notes: usize, uid: usize, shn: usize) -> String {
    let who = |by: &str, at: &str| {
        format!(
            "CASE WHEN c.{at} IS NULL THEN NULL ELSE concat_ws(' · ', (SELECT display_name FROM users WHERE id = c.{by}), \
             to_char(c.{at} AT TIME ZONE ${tz}, 'DD.MM.YY HH24:MI')) END"
        )
    };
    format!(
        "c.id, c.name, c.organization, c.phone, c.mobile, c.email, c.note, c.user_id IS NULL AS shared,
         CASE WHEN ${notes} THEN (SELECT count(*) FROM call_notes n WHERE n.e164 IN (c.phone_e164, c.mobile_e164)
                                  AND (n.user_id = ${uid} OR (n.shared AND ${shn}))) ELSE 0 END AS notes,
         CASE WHEN c.user_id IS NULL THEN {} END AS created, CASE WHEN c.user_id IS NULL THEN {} END AS updated,
         {} AS deleted",
        who("created_by", "created_at"),
        who("updated_by", "updated_at"),
        who("deleted_by", "deleted_at"),
    )
}

/// Own personal contacts (phone book page).
pub async fn contacts_of(app: &AppState, uid: i32, may_phone: bool) -> Result<Vec<Contact>, AppError> {
    Ok(sqlx::query_as(&format!("SELECT {} FROM contacts c WHERE c.user_id = $1 ORDER BY lower(c.name)", contact_cols(2, 3, 1, 4)))
        .bind(uid)
        .bind(&app.cfg.timezone)
        .bind(may_phone)
        .bind(app.cfg.shared_notes)
        .fetch_all(&app.db)
        .await
        .map_err(anyhow::Error::from)?)
}

/// The shared phone book (empty while PBX_SHARED_CONTACTS=off).
pub async fn shared_contacts(app: &AppState, u: &CurrentUser, uid: i32) -> Result<Vec<Contact>, AppError> {
    if !shared_access(app, u).0 {
        return Ok(vec![]);
    }
    let may_phone = u.may_phone;
    Ok(sqlx::query_as(&format!("SELECT {} FROM contacts c WHERE c.user_id IS NULL AND c.deleted_at IS NULL ORDER BY lower(c.name)", contact_cols(2, 3, 1, 4)))
        .bind(uid)
        .bind(&app.cfg.timezone)
        .bind(may_phone)
        .bind(app.cfg.shared_notes)
        .fetch_all(&app.db)
        .await
        .map_err(anyhow::Error::from)?)
}

/// Bin of the shared phone book (admins).
pub async fn shared_bin(app: &AppState) -> Result<Vec<Contact>, AppError> {
    Ok(sqlx::query_as(&format!(
        "SELECT {} FROM contacts c WHERE c.user_id IS NULL AND c.deleted_at IS NOT NULL ORDER BY c.deleted_at DESC",
        contact_cols(1, 2, 3, 2)
    ))
    .bind(&app.cfg.timezone)
    .bind(false)
    .bind(0)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?)
}

/// A contact the viewer may see: own, or shared while the phone book is on (not in the bin).
pub(crate) async fn load(app: &AppState, u: &CurrentUser, uid: i32, id: i32) -> Result<Contact, AppError> {
    sqlx::query_as(&format!(
        "SELECT {} FROM contacts c WHERE c.id = $5 AND (c.user_id = $1 OR ($6 AND c.user_id IS NULL AND c.deleted_at IS NULL))",
        contact_cols(2, 3, 1, 4)
    ))
    .bind(uid)
    .bind(&app.cfg.timezone)
    .bind(false)
    .bind(false)
    .bind(id)
    .bind(shared_access(app, u).0)
    .fetch_optional(&app.db)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or(AppError::NotFound)
}

/// Shared phone book for this user: (may read, may edit). Phone permission is required ("everyone
/// with phone access"); admins always see it.
pub(crate) fn shared_access(app: &AppState, u: &CurrentUser) -> (bool, bool) {
    let read = app.cfg.shared_contacts.on() && (u.may_phone || u.is_admin);
    (read, read && app.cfg.shared_contacts.may_edit(u.is_admin))
}

pub(crate) fn may_edit(app: &AppState, u: &CurrentUser, k: &Contact) -> bool {
    !k.shared || shared_access(app, u).1
}

#[derive(Template)]
#[template(path = "contact.html")]
struct ContactPage {
    c: Ctx,
    k: Contact,
    is_new: bool,
    /// the viewer may change it (own, or shared with edit permission)
    editable: bool,
    /// new contact: offer "shared phone book" as the place
    offer_shared: bool,
    /// own contact: offer "move to the shared phone book"
    may_share: bool,
    /// shared contact: offer "copy to my contacts"
    may_copy: bool,
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
    /// "shared": preselect the shared phone book
    #[serde(default)]
    scope: String,
}

/// New contact; `?phone=…&name=…` prefill it (call companion "Save as contact").
pub async fn contact_new(State(app): State<AppState>, u: CurrentUser, axum::extract::Query(p): axum::extract::Query<ContactPrefill>) -> Result<Response, AppError> {
    u.phone_uid()?;
    let phone: String = p.phone.chars().filter(|c| c.is_ascii_digit() || *c == '+').take(20).collect();
    let name: String = p.name.chars().filter(|c| !c.is_control()).take(80).collect();
    let offer_shared = shared_access(&app, &u).1;
    let k = Contact { phone: Some(phone).filter(|s| !s.is_empty()), name, shared: offer_shared && p.scope == "shared", ..Default::default() };
    render(ContactPage {
        c: Ctx::new(&app, &u),
        k,
        is_new: true,
        editable: true,
        offer_shared,
        may_share: false,
        may_copy: false,
        notes: vec![],
        note_numbers: vec![],
        shared_notes: false,
    })
}

pub async fn contact_edit(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let k = load(&app, &u, uid, id).await?;
    // call notes only with phone permission (like the call window)
    let note_numbers = if u.may_phone { contact_numbers(&app, &u, uid, id).await? } else { vec![] };
    let notes = crate::notes::notes_for_numbers(&app, uid, &note_numbers).await?;
    let editable = may_edit(&app, &u, &k);
    render(ContactPage {
        c: Ctx::new(&app, &u),
        editable,
        offer_shared: false,
        may_share: !k.shared && shared_access(&app, &u).1,
        may_copy: k.shared,
        k,
        is_new: false,
        notes,
        note_numbers,
        shared_notes: app.cfg.shared_notes,
    })
}

/// External numbers (E.164) of a contact the viewer may see.
async fn contact_numbers(app: &AppState, u: &CurrentUser, uid: i32, id: i32) -> Result<Vec<String>, AppError> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT phone_e164, mobile_e164 FROM contacts
         WHERE id = $1 AND (user_id = $2 OR ($3 AND user_id IS NULL AND deleted_at IS NULL))",
    )
    .bind(id)
    .bind(uid)
    .bind(shared_access(app, u).0)
    .fetch_optional(&app.db)
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
    if !contact_numbers(&app, &u, uid, id).await?.contains(&f.number) {
        return Err(AppError::BadRequest("Notes only for the contact's external numbers.".into()));
    }
    let form = crate::notes::NoteForm { number: f.number, body: f.body, shared: f.shared };
    crate::notes::save(&app, &u, uid, None, &form).await?;
    Ok(Redirect::to(&format!("/contacts/{id}#notes")).into_response())
}

/// POST /contacts/{id}/notes/{note}/delete: own notes only.
pub async fn contact_note_delete(State(app): State<AppState>, u: CurrentUser, Path((id, note)): Path<(i32, i32)>) -> Result<Response, AppError> {
    let uid = crate::companion::uid(&u)?;
    crate::notes::delete_own(&app, &u, uid, note).await?;
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
    /// new contacts: "shared" for the shared phone book
    #[serde(default)]
    scope: String,
}

pub(crate) fn opt(s: &str, max: usize) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.chars().take(max).collect())
}

/// Phone numbers: external numbers are stored as typed plus E.164 for matching; short internal numbers as digits.
pub(crate) fn number(label: &str, s: &str) -> Result<(Option<String>, Option<String>), AppError> {
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

fn back(shared: bool) -> &'static str {
    if shared { "/phonebook#shared" } else { "/phonebook#contacts" }
}

pub async fn contact_save(State(app): State<AppState>, u: CurrentUser, Path(id): Path<String>, Form(f): Form<ContactForm>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let name = opt(&f.name, 80).ok_or_else(|| AppError::BadRequest("Name missing.".into()))?;
    let (phone, phone_e164) = number("Phone", &f.phone)?;
    let (mobile, mobile_e164) = number("Mobile", &f.mobile)?;
    let existing = match id.as_str() {
        "new" => None,
        s => Some(load(&app, &u, uid, s.parse().map_err(|_| AppError::NotFound)?).await?),
    };
    let shared = match &existing {
        Some(k) => {
            if !may_edit(&app, &u, k) {
                return Err(AppError::Forbidden("Only admins change the shared phone book here.".into()));
            }
            k.shared
        }
        None => f.scope == "shared" && shared_access(&app, &u).1,
    };
    // Contacts synced from a phone (CardDAV) keep their stored card; the edit is applied to it.
    let synced = match &existing {
        Some(k) => crate::carddav::contact_card(&app.db, (!shared).then_some(uid), k.id).await?.and_then(|(f, v)| v.map(|v| (f, v))),
        None => None,
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
    let q = if existing.is_none() {
        let (count, max): (i64, i64) = if shared {
            let n = sqlx::query_scalar("SELECT count(*) FROM contacts WHERE user_id IS NULL AND deleted_at IS NULL").fetch_one(&app.db).await;
            (n.map_err(anyhow::Error::from)?, MAX_SHARED)
        } else {
            let n = sqlx::query_scalar("SELECT count(*) FROM contacts WHERE user_id = $1").bind(uid).fetch_one(&app.db).await;
            (n.map_err(anyhow::Error::from)?, crate::carddav::MAX_CONTACTS)
        };
        if count >= max {
            return Err(AppError::BadRequest(format!("At most {max} contacts.")));
        }
        "INSERT INTO contacts (user_id, name, organization, phone, phone_e164, mobile, mobile_e164, email, note, created_by, updated_by)
         VALUES (CASE WHEN $12 THEN NULL ELSE $2 END, $3, $4, $5, $6, $7, $8, $9, $10, $2, $2) RETURNING id"
    } else {
        "UPDATE contacts SET name = $3, organization = $4, phone = $5, phone_e164 = $6, mobile = $7, mobile_e164 = $8,
                email = $9, note = $10, vcard = COALESCE($11, vcard), updated_at = now(), updated_by = $2
         WHERE id = $1 AND (user_id = $2 OR ($12 AND user_id IS NULL AND deleted_at IS NULL)) RETURNING id"
    };
    let saved: i32 = sqlx::query_scalar::<_, i32>(q)
        .bind(existing.as_ref().map(|k| k.id).unwrap_or(0))
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
        .bind(shared)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or(AppError::NotFound)?;
    if shared {
        let action = if existing.is_none() { "contact.shared.create" } else { "contact.shared.update" };
        crate::db::audit(&app.db, &u.session.sub, action, &format!("contact:{saved}"), serde_json::json!({"name": name})).await?;
    }
    Ok(Redirect::to(back(shared)).into_response())
}

pub async fn contact_create(state: State<AppState>, u: CurrentUser, form: Form<ContactForm>) -> Result<Response, AppError> {
    contact_save(state, u, Path("new".to_string()), form).await
}

#[derive(Deserialize, Default)]
pub struct DeleteForm {
    /// "dupes": back to the duplicate finder (with its criteria in `q`)
    #[serde(default)]
    back: String,
}

/// Personal contacts are deleted; shared ones go to the bin.
pub async fn contact_delete(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>, Form(form): Form<DeleteForm>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let k = load(&app, &u, uid, id).await?;
    if !may_edit(&app, &u, &k) {
        return Err(AppError::Forbidden("Only admins change the shared phone book here.".into()));
    }
    let mut conn = app.db.acquire().await.map_err(anyhow::Error::from)?;
    remove(&app, &mut conn, &u, uid, &k).await?;
    let to = if form.back.starts_with("/contacts/duplicates") { crate::dupes::back_to(&form.back) } else { back(k.shared).to_string() };
    Ok(Redirect::to(&to).into_response())
}

/// Deletes an own contact, or moves a shared one to the bin (the caller checked `may_edit`).
/// Returns whether a row was removed (false: already gone or binned meanwhile).
pub(crate) async fn remove(app: &AppState, conn: &mut sqlx::PgConnection, u: &CurrentUser, uid: i32, k: &Contact) -> Result<bool, AppError> {
    let n = if k.shared {
        sqlx::query("UPDATE contacts SET deleted_at = now(), deleted_by = $2 WHERE id = $1 AND user_id IS NULL AND deleted_at IS NULL")
            .bind(k.id)
            .bind(uid)
            .execute(&mut *conn)
            .await
            .map_err(anyhow::Error::from)?
            .rows_affected()
    } else {
        sqlx::query("DELETE FROM contacts WHERE id = $1 AND user_id = $2").bind(k.id).bind(uid).execute(&mut *conn).await.map_err(anyhow::Error::from)?.rows_affected()
    };
    if n > 0 && k.shared {
        crate::db::audit(&app.db, &u.session.sub, "contact.shared.delete", &format!("contact:{}", k.id), serde_json::json!({"name": k.name})).await?;
    }
    Ok(n > 0)
}

/// New values for an existing contact the viewer may change (merge); a card synced from a phone is
/// patched like an edit in the form.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn update_fields(
    app: &AppState,
    conn: &mut sqlx::PgConnection,
    uid: i32,
    k: &Contact,
    name: &str,
    organization: &str,
    phone: &str,
    mobile: &str,
    email: &str,
    note: &str,
    // values already stored in one of the contacts: kept as they are even if the web form would
    // reject them (CardDAV stores numbers like "+49.40.123" or "0800-FLOWERS")
    stored: &[String],
) -> Result<bool, AppError> {
    let name = opt(name, 80).ok_or_else(|| AppError::BadRequest("Name missing.".into()))?;
    let lenient = |label: &str, v: &str| match number(label, v) {
        Err(_) if !v.trim().is_empty() && stored.iter().any(|s| s == v) => Ok((Some(v.to_string()), to_e164(v))),
        r => r,
    };
    let (phone, phone_e164) = lenient("Phone", phone)?;
    let (mobile, mobile_e164) = lenient("Mobile", mobile)?;
    let fields = crate::carddav::Fields {
        name: name.clone(),
        organization: opt(organization, 80),
        phone: phone.clone(),
        mobile: mobile.clone(),
        email: opt(email, 120),
        note: opt(note, 500),
    };
    let synced = crate::carddav::contact_card(&app.db, (!k.shared).then_some(uid), k.id).await?.and_then(|(f, v)| v.map(|v| (f, v)));
    let vcard = synced.map(|(old, v)| crate::carddav::patch_vcard(&v, &old, &fields));
    let r = sqlx::query(
        "UPDATE contacts SET name = $3, organization = $4, phone = $5, phone_e164 = $6, mobile = $7, mobile_e164 = $8,
                email = $9, note = $10, vcard = COALESCE($11, vcard), updated_at = now(), updated_by = $2
         WHERE id = $1 AND (user_id = $2 OR ($12 AND user_id IS NULL AND deleted_at IS NULL))",
    )
    .bind(k.id)
    .bind(uid)
    .bind(&name)
    .bind(&fields.organization)
    .bind(&phone)
    .bind(&phone_e164)
    .bind(&mobile)
    .bind(&mobile_e164)
    .bind(&fields.email)
    .bind(&fields.note)
    .bind(vcard)
    .bind(k.shared)
    .execute(&mut *conn)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(n_ok(r.rows_affected()))
}

fn n_ok(n: u64) -> bool {
    n == 1
}

/// POST /contacts/{id}/share: an own contact moves to the shared phone book.
pub async fn contact_share(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    if !shared_access(&app, &u).1 {
        return Err(AppError::Forbidden("Only admins change the shared phone book here.".into()));
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM contacts WHERE user_id IS NULL AND deleted_at IS NULL")
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    if count >= MAX_SHARED {
        return Err(AppError::BadRequest(format!("At most {MAX_SHARED} contacts.")));
    }
    // the CardDAV name goes: it identified the card in the person's own book
    let name: Option<String> = sqlx::query_scalar(
        // the stored card goes too: fields the web UI does not show (photo, addresses, birthday) stay
        // private, and the card gets a new UID; the shared entry is rebuilt from the visible fields
        "UPDATE contacts SET user_id = NULL, dav_name = NULL, vcard = NULL, created_by = COALESCE(created_by, $2), updated_by = $2, updated_at = now()
         WHERE id = $1 AND user_id = $2 RETURNING name",
    )
    .bind(id)
    .bind(uid)
    .fetch_optional(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let name = name.ok_or(AppError::NotFound)?;
    crate::db::audit(&app.db, &u.session.sub, "contact.shared.move", &format!("contact:{id}"), serde_json::json!({"name": name})).await?;
    Ok(Redirect::to(&format!("/contacts/{id}")).into_response())
}

/// POST /contacts/{id}/copy: a shared contact is copied to the own contacts.
pub async fn contact_copy(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM contacts WHERE user_id = $1").bind(uid).fetch_one(&app.db).await.map_err(anyhow::Error::from)?;
    if count >= crate::carddav::MAX_CONTACTS {
        return Err(AppError::BadRequest(format!("At most {} contacts.", crate::carddav::MAX_CONTACTS)));
    }
    let new: Option<i32> = sqlx::query_scalar(
        // the visible fields only: a copy of the stored card would carry the same vCard UID
        "INSERT INTO contacts (user_id, name, organization, phone, phone_e164, mobile, mobile_e164, email, note, created_by, updated_by)
         SELECT $2, name, organization, phone, phone_e164, mobile, mobile_e164, email, note, $2, $2
         FROM contacts WHERE id = $1 AND user_id IS NULL AND deleted_at IS NULL AND $3 RETURNING id",
    )
    .bind(id)
    .bind(uid)
    .bind(shared_access(&app, &u).0)
    .fetch_optional(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let new = new.ok_or(AppError::NotFound)?;
    Ok(Redirect::to(&format!("/contacts/{new}")).into_response())
}

/// POST /contacts/{id}/restore: admins take a shared contact out of the bin.
pub async fn contact_restore(State(app): State<AppState>, u: CurrentUser, Path(id): Path<i32>) -> Result<Response, AppError> {
    if !u.is_admin {
        return Err(AppError::Forbidden("Admins only.".into()));
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM contacts WHERE user_id IS NULL AND deleted_at IS NULL")
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    if count >= MAX_SHARED {
        return Err(AppError::BadRequest(format!("At most {MAX_SHARED} contacts.")));
    }
    // a live entry may have taken its CardDAV name meanwhile: the restored one gets c<id>.vcf
    let name: Option<String> = sqlx::query_scalar(
        "UPDATE contacts c SET deleted_at = NULL, deleted_by = NULL, updated_at = now(),
                dav_name = CASE WHEN EXISTS (SELECT 1 FROM contacts o WHERE o.user_id IS NULL AND o.deleted_at IS NULL AND o.dav_name = c.dav_name)
                                THEN NULL ELSE c.dav_name END
         WHERE c.id = $1 AND c.user_id IS NULL AND c.deleted_at IS NOT NULL RETURNING c.name",
    )
    .bind(id)
    .fetch_optional(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let name = name.ok_or(AppError::NotFound)?;
    crate::db::audit(&app.db, &u.session.sub, "contact.shared.restore", &format!("contact:{id}"), serde_json::json!({"name": name})).await?;
    Ok(Redirect::to("/phonebook#bin").into_response())
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
                  -- own contacts first, then the shared phone book (settings.shared_contacts)
                  -- Name (Company) like on the phones (settings.caller_name_format)
                  (SELECT CASE WHEN fmt.v = 'name_org' AND COALESCE(btrim(k.organization), '') NOT IN ('', btrim(k.name))
                               THEN k.name || ' (' || btrim(k.organization) || ')' ELSE k.name END FROM contacts k
                   WHERE (k.user_id = $1 OR (k.user_id IS NULL AND k.deleted_at IS NULL
                                             AND COALESCE((SELECT value FROM settings WHERE key = 'shared_contacts'), 'off') <> 'off'))
                     AND (k.phone = c.number OR k.mobile = c.number
                       OR k.phone_e164 = c.number OR k.mobile_e164 = c.number
                       OR k.phone_e164 = nat.e164 OR k.mobile_e164 = nat.e164)
                   ORDER BY k.user_id IS NULL, k.id LIMIT 1),
                  (SELECT x.display_name FROM users x WHERE x.extension::text = c.number AND x.deleted_at IS NULL),
                  -- then the external phone books: external numbers only (E.164), never an internal one
                  (SELECT CASE WHEN fmt.v = 'name_org' AND COALESCE(btrim(x.organization), '') NOT IN ('', btrim(x.name))
                               THEN x.name || ' (' || btrim(x.organization) || ')' ELSE x.name END FROM external_contacts x
                   WHERE x.phone_e164 IN (c.number, nat.e164) OR x.mobile_e164 IN (c.number, nat.e164)
                   ORDER BY x.book, x.res LIMIT 1)) AS name
         FROM calls c
         -- national number (0…) as E.164 with the site's country code (settings.country_code, PBX_COUNTRY_CODE)
         LEFT JOIN LATERAL (SELECT '+' || s.value || substr(c.number, 2) AS e164 FROM settings s
                            WHERE s.key = 'country_code' AND c.number LIKE '0%' AND c.number NOT LIKE '00%') nat ON true
         CROSS JOIN (SELECT COALESCE((SELECT value FROM settings WHERE key = 'caller_name_format'), 'name') AS v) fmt
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
