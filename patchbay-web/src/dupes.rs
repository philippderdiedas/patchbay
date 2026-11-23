// SPDX-License-Identifier: GPL-3.0-or-later
//! Duplicate finder for contacts: the person's own contacts and the shared phone book, compared by
//! phone numbers (E.164, internal numbers as digits), e-mail and similar names (checkboxes). Per pair:
//! merge (choose each field, the other entry is deleted; a shared one goes to the bin), delete one,
//! or "not a duplicate" (remembered per person in contact_dupe_ignores, undone on the same page).
//! Other people's personal contacts are never compared.

use std::collections::{HashMap, HashSet};

use askama::Template;
use axum::{
    Form,
    extract::{Query, State},
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use sqlx::FromRow;

use crate::{
    AppError, AppState,
    auth::CurrentUser,
    contacts::{Contact, may_edit},
    ui::{Ctx, render},
};

/// Pairs shown at most (the rest after resolving these)
const MAX_PAIRS: usize = 200;

#[derive(FromRow, Clone)]
struct Row {
    id: i32,
    name: String,
    organization: Option<String>,
    phone: Option<String>,
    phone_e164: Option<String>,
    mobile: Option<String>,
    mobile_e164: Option<String>,
    email: Option<String>,
    shared: bool,
    synced: bool,
}

impl Row {
    /// Comparable numbers: E.164 for external ones, digits for internal ones
    fn numbers(&self) -> Vec<String> {
        let one = |raw: &Option<String>, e: &Option<String>| match (raw, e) {
            (_, Some(e)) => Some(e.clone()),
            (Some(r), None) => Some(r.chars().filter(|c| c.is_ascii_digit()).collect::<String>()).filter(|d| !d.is_empty()),
            _ => None,
        };
        let mut v: Vec<String> = [one(&self.phone, &self.phone_e164), one(&self.mobile, &self.mobile_e164)].into_iter().flatten().collect();
        v.dedup();
        v
    }
}

/// Name for comparing: lower case, umlauts folded, punctuation dropped, words sorted
/// ("Müller, Hans" = "hans mueller").
pub fn name_key(name: &str) -> String {
    let folded: String = name
        .to_lowercase()
        .chars()
        .flat_map(|c| match c {
            'ä' => "ae".chars().collect::<Vec<_>>(),
            'ö' => "oe".chars().collect(),
            'ü' => "ue".chars().collect(),
            'ß' => "ss".chars().collect(),
            c if c.is_alphanumeric() => vec![c],
            _ => vec![' '],
        })
        .collect();
    let mut words: Vec<&str> = folded.split_whitespace().collect();
    words.sort_unstable();
    words.join(" ")
}

/// Edit distance (Levenshtein), stops early above `max`
fn distance(a: &str, b: &str, max: usize) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len().abs_diff(b.len()) > max {
        return max + 1;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != cb)).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        if cur.iter().min().copied().unwrap_or(0) > max {
            return max + 1;
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Names that are probably the same person: same words, or (long enough) at most 2 typos apart.
pub fn similar_names(a: &str, b: &str) -> bool {
    let (ka, kb) = (name_key(a), name_key(b));
    if ka.is_empty() || kb.is_empty() {
        return false;
    }
    ka == kb || (ka.chars().count() >= 6 && kb.chars().count() >= 6 && distance(&ka, &kb, 2) <= 2)
}

#[derive(Deserialize)]
pub struct Criteria {
    /// checkboxes; a missing box is off once the form was sent (`sent`)
    #[serde(default)]
    numbers: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    names: Option<String>,
    #[serde(default)]
    sent: Option<String>,
}

impl Criteria {
    fn flags(&self) -> (bool, bool, bool) {
        if self.sent.is_none() {
            return (true, true, true);
        }
        (self.numbers.is_some(), self.email.is_some(), self.names.is_some())
    }
}

pub struct Pair {
    pub a: Contact,
    pub b: Contact,
    /// "same number", "same e-mail", "similar name"
    pub reasons: Vec<&'static str>,
    /// the viewer may merge or delete each side
    pub edit_a: bool,
    pub edit_b: bool,
}

impl Pair {
    pub fn sides(&self) -> Vec<(&Contact, bool)> {
        vec![(&self.a, self.edit_a), (&self.b, self.edit_b)]
    }
}

#[derive(Template)]
#[template(path = "duplicates.html")]
struct DupesPage {
    c: Ctx,
    numbers: bool,
    email: bool,
    names: bool,
    pairs: Vec<Pair>,
    more: bool,
    ignored: Vec<(Contact, Contact)>,
    shared_on: bool,
    /// this page with its criteria (target after delete/ignore/merge)
    back: String,
}

async fn rows(app: &AppState, u: &CurrentUser, uid: i32) -> Result<Vec<Row>, AppError> {
    Ok(sqlx::query_as(
        "SELECT id, name, organization, phone, phone_e164, mobile, mobile_e164, email, user_id IS NULL AS shared, vcard IS NOT NULL AS synced
         FROM contacts WHERE user_id = $1 OR ($2 AND user_id IS NULL AND deleted_at IS NULL) ORDER BY id",
    )
    .bind(uid)
    .bind(crate::contacts::shared_access(app, u).0)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?)
}

fn contact(r: &Row) -> Contact {
    Contact {
        id: r.id,
        name: r.name.clone(),
        organization: r.organization.clone(),
        phone: r.phone.clone(),
        mobile: r.mobile.clone(),
        email: r.email.clone(),
        shared: r.shared,
        ..Default::default()
    }
}

/// Candidate pairs (a.id < b.id) with their reasons.
fn find(rows: &[Row], numbers: bool, email: bool, names: bool) -> Vec<(usize, usize, Vec<&'static str>)> {
    let mut reasons: HashMap<(usize, usize), Vec<&'static str>> = HashMap::new();
    let mut add = |i: usize, j: usize, why: &'static str| {
        let k = if i < j { (i, j) } else { (j, i) };
        let v = reasons.entry(k).or_default();
        if !v.contains(&why) {
            v.push(why);
        }
    };
    let group = |keys: Vec<(String, usize)>, why: &'static str, add: &mut dyn FnMut(usize, usize, &'static str)| {
        let mut by: HashMap<String, Vec<usize>> = HashMap::new();
        for (k, i) in keys {
            by.entry(k).or_default().push(i);
        }
        for v in by.values().filter(|v| v.len() > 1 && v.len() <= 20) {
            for (x, &i) in v.iter().enumerate() {
                for &j in &v[x + 1..] {
                    if i != j {
                        add(i, j, why);
                    }
                }
            }
        }
    };
    if numbers {
        let keys = rows.iter().enumerate().flat_map(|(i, r)| r.numbers().into_iter().filter(|n| n.len() > 2).map(move |n| (n, i))).collect();
        group(keys, "same number", &mut add);
    }
    if email {
        let keys = rows.iter().enumerate().filter_map(|(i, r)| r.email.as_ref().map(|e| (e.trim().to_lowercase(), i))).filter(|(e, _)| !e.is_empty()).collect();
        group(keys, "same e-mail", &mut add);
    }
    if names {
        // compare within buckets of the first two letters of the sorted name (typos rarely hit them)
        let mut buckets: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, r) in rows.iter().enumerate() {
            let k: String = name_key(&r.name).chars().take(2).collect();
            buckets.entry(k).or_default().push(i);
        }
        for v in buckets.values() {
            for (x, &i) in v.iter().enumerate() {
                for &j in &v[x + 1..] {
                    if similar_names(&rows[i].name, &rows[j].name) {
                        add(i, j, "similar name");
                    }
                }
            }
        }
    }
    let mut out: Vec<(usize, usize, Vec<&'static str>)> = reasons.into_iter().map(|((i, j), r)| (i, j, r)).collect();
    // definite matches (number, e-mail) first
    out.sort_by_key(|(i, j, r)| (r == &vec!["similar name"], rows[*i].name.to_lowercase(), *j));
    out
}

/// GET /contacts/duplicates?numbers=on&email=on&names=on&sent=1
pub async fn page(State(app): State<AppState>, u: CurrentUser, Query(q): Query<Criteria>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let (numbers, email, names) = q.flags();
    let rows = rows(&app, &u, uid).await?;
    let ignored_ids: Vec<(i32, i32)> =
        sqlx::query_as("SELECT a, b FROM contact_dupe_ignores WHERE user_id = $1 ORDER BY created_at DESC").bind(uid).fetch_all(&app.db).await.map_err(anyhow::Error::from)?;
    let ignored_set: HashSet<(i32, i32)> = ignored_ids.iter().copied().collect();
    let all = find(&rows, numbers, email, names);
    let mut pairs = vec![];
    let mut more = false;
    for (i, j, reasons) in all {
        let (a, b) = (&rows[i], &rows[j]);
        let key = (a.id.min(b.id), a.id.max(b.id));
        if ignored_set.contains(&key) {
            continue;
        }
        if pairs.len() >= MAX_PAIRS {
            more = true;
            break;
        }
        let (ca, cb) = (contact(a), contact(b));
        pairs.push(Pair { edit_a: may_edit(&app, &u, &ca), edit_b: may_edit(&app, &u, &cb), a: ca, b: cb, reasons });
    }
    let by_id: HashMap<i32, &Row> = rows.iter().map(|r| (r.id, r)).collect();
    let ignored = ignored_ids.iter().filter_map(|(a, b)| Some((contact(by_id.get(a)?), contact(by_id.get(b)?)))).collect();
    let back = format!(
        "/contacts/duplicates?sent=1{}{}{}",
        if numbers { "&numbers=on" } else { "" },
        if email { "&email=on" } else { "" },
        if names { "&names=on" } else { "" }
    );
    render(DupesPage { c: Ctx::new(&app, &u), numbers, email, names, pairs, more, ignored, shared_on: crate::contacts::shared_access(&app, &u).0, back })
}

#[derive(Deserialize)]
pub struct PairForm {
    a: i32,
    b: i32,
    #[serde(default)]
    back: String,
}

pub(crate) fn back_to(back: &str) -> String {
    // only the duplicates page with its criteria (no open redirect)
    match back.strip_prefix("/contacts/duplicates") {
        Some(rest) if rest.is_empty() || (rest.starts_with('?') && rest.chars().all(|c| c.is_ascii_alphanumeric() || "?&=_".contains(c))) => back.to_string(),
        _ => "/contacts/duplicates".into(),
    }
}

/// POST /contacts/duplicates/ignore (a, b): "not a duplicate"
pub async fn ignore(State(app): State<AppState>, u: CurrentUser, Form(f): Form<PairForm>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let visible: HashSet<i32> = rows(&app, &u, uid).await?.iter().map(|r| r.id).collect();
    if f.a == f.b || !visible.contains(&f.a) || !visible.contains(&f.b) {
        return Err(AppError::NotFound);
    }
    sqlx::query("INSERT INTO contact_dupe_ignores (user_id, a, b) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
        .bind(uid)
        .bind(f.a.min(f.b))
        .bind(f.a.max(f.b))
        .execute(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(Redirect::to(&back_to(&f.back)).into_response())
}

/// POST /contacts/duplicates/unignore (a, b): undo "not a duplicate"
pub async fn unignore(State(app): State<AppState>, u: CurrentUser, Form(f): Form<PairForm>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    sqlx::query("DELETE FROM contact_dupe_ignores WHERE user_id = $1 AND a = $2 AND b = $3")
        .bind(uid)
        .bind(f.a.min(f.b))
        .bind(f.a.max(f.b))
        .execute(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(Redirect::to(&back_to(&f.back)).into_response())
}

// --- Merge ----------------------------------------------------------------------------------------

pub const FIELDS: [&str; 6] = ["name", "organization", "phone", "mobile", "email", "note"];

#[derive(Template)]
#[template(path = "merge.html")]
struct MergePage {
    c: Ctx,
    a: Contact,
    b: Contact,
    /// may the viewer keep (change) a / b; the other one is deleted
    keep_a: bool,
    keep_b: bool,
    /// the entry that would be deleted holds a card synced from a phone (fields beyond these are lost)
    synced_a: bool,
    synced_b: bool,
    back: String,
}

impl MergePage {
    /// (label, field, value of a, value of b)
    fn rows(&self) -> Vec<(&'static str, &'static str, String, String)> {
        let v = |k: &Contact, f: &str| match f {
            "name" => k.name.clone(),
            "organization" => k.org(),
            "phone" => k.phone_str(),
            "mobile" => k.mobile_str(),
            "email" => k.email_str(),
            _ => k.note_str(),
        };
        let labels = ["Name", "Company", "Phone", "Mobile", "E-mail", "Note"];
        FIELDS.iter().zip(labels).map(|(f, l)| (l, *f, v(&self.a, f), v(&self.b, f))).collect()
    }
}

#[derive(Deserialize)]
pub struct MergeQuery {
    a: i32,
    b: i32,
    #[serde(default)]
    back: String,
}

/// GET /contacts/merge?a=&b=
pub async fn merge_page(State(app): State<AppState>, u: CurrentUser, Query(q): Query<MergeQuery>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    if q.a == q.b {
        return Err(AppError::BadRequest("Two different contacts.".into()));
    }
    let a = crate::contacts::load(&app, &u, uid, q.a).await?;
    let b = crate::contacts::load(&app, &u, uid, q.b).await?;
    let (ea, eb) = (may_edit(&app, &u, &a), may_edit(&app, &u, &b));
    // merging changes one entry and deletes the other: both need edit rights
    if !(ea && eb) {
        return Err(AppError::Forbidden("Only admins change the shared phone book here.".into()));
    }
    let synced: HashSet<i32> = rows(&app, &u, uid).await?.iter().filter(|r| r.synced).map(|r| r.id).collect();
    render(MergePage {
        c: Ctx::new(&app, &u),
        keep_a: ea,
        keep_b: eb,
        synced_a: synced.contains(&a.id),
        synced_b: synced.contains(&b.id),
        a,
        b,
        back: back_to(&q.back),
    })
}

/// POST /contacts/merge: keep (id), drop (id), and per field "a" or "b"
pub async fn merge(State(app): State<AppState>, u: CurrentUser, Form(f): Form<HashMap<String, String>>) -> Result<Response, AppError> {
    let uid = u.phone_uid()?;
    let id = |k: &str| f.get(k).and_then(|v| v.parse::<i32>().ok()).ok_or_else(|| AppError::BadRequest(format!("{k} missing.")));
    let (ia, ib, keep) = (id("a")?, id("b")?, id("keep")?);
    if ia == ib || (keep != ia && keep != ib) {
        return Err(AppError::BadRequest("Two different contacts.".into()));
    }
    let a = crate::contacts::load(&app, &u, uid, ia).await?;
    let b = crate::contacts::load(&app, &u, uid, ib).await?;
    if !(may_edit(&app, &u, &a) && may_edit(&app, &u, &b)) {
        return Err(AppError::Forbidden("Only admins change the shared phone book here.".into()));
    }
    let pick = |field: &str, va: String, vb: String| if f.get(field).map(String::as_str) == Some("b") { vb } else { va };
    let name = pick("name", a.name.clone(), b.name.clone());
    let organization = pick("organization", a.org(), b.org());
    let phone = pick("phone", a.phone_str(), b.phone_str());
    let mobile = pick("mobile", a.mobile_str(), b.mobile_str());
    let email = pick("email", a.email_str(), b.email_str());
    let note = pick("note", a.note_str(), b.note_str());
    let (target, other) = if keep == ia { (a, b) } else { (b, a) };
    crate::contacts::update_fields(&app, uid, &target, &name, &organization, &phone, &mobile, &email, &note).await?;
    crate::contacts::remove(&app, &u, uid, &other).await?;
    if target.shared || other.shared {
        crate::db::audit(
            &app.db,
            &u.session.sub,
            "contact.merge",
            &format!("contact:{}", target.id),
            serde_json::json!({"removed": other.id, "name": name}),
        )
        .await?;
    }
    let back = f.get("back").map(|b| back_to(b)).unwrap_or_else(|| "/contacts/duplicates".into());
    Ok(Redirect::to(&back).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i32, name: &str, phone: Option<&str>, e164: Option<&str>, email: Option<&str>) -> Row {
        Row {
            id,
            name: name.into(),
            organization: None,
            phone: phone.map(Into::into),
            phone_e164: e164.map(Into::into),
            mobile: None,
            mobile_e164: None,
            email: email.map(Into::into),
            shared: false,
            synced: false,
        }
    }

    #[test]
    fn names() {
        assert_eq!(name_key("Müller, Hans"), "hans mueller");
        assert!(similar_names("Hans Müller", "Müller, Hans"));
        assert!(similar_names("Hans Mueller", "Hans Müler"));
        assert!(!similar_names("Hans Müller", "Hans Meier"));
        assert!(!similar_names("Bo", "Bob"), "short names only when equal");
        assert_eq!(distance("kitten", "sitting", 5), 3);
    }

    #[test]
    fn pairs_by_criteria() {
        let rows = vec![
            row(1, "Lieferant GmbH", Some("040 123456"), Some("+4940123456"), None),
            row(2, "Lieferant", Some("+49 40 123456"), Some("+4940123456"), Some("info@x.de")),
            row(3, "Anna Schmidt", None, None, Some("INFO@x.de ")),
            row(4, "Schmidt Anna", Some("1015"), None, None),
            row(5, "Intern", Some("10-15"), None, None),
        ];
        let all = find(&rows, true, true, true);
        let has = |a: i32, b: i32, why: &str| all.iter().any(|(i, j, r)| rows[*i].id == a && rows[*j].id == b && r.contains(&why));
        assert!(has(1, 2, "same number"));
        assert!(has(2, 3, "same e-mail"));
        assert!(has(3, 4, "similar name"));
        assert!(has(4, 5, "same number"), "internal numbers as digits");
        // numbers only
        let only = find(&rows, true, false, false);
        assert!(only.iter().all(|(_, _, r)| r == &vec!["same number"]));
        assert_eq!(find(&rows, false, false, false).len(), 0);
    }

    #[test]
    fn redirects_stay_on_the_page() {
        assert_eq!(back_to("/contacts/duplicates?numbers=on&sent=1"), "/contacts/duplicates?numbers=on&sent=1");
        assert_eq!(back_to("https://evil.example/"), "/contacts/duplicates");
        assert_eq!(back_to("/contacts/duplicates/../x"), "/contacts/duplicates");
    }
}
