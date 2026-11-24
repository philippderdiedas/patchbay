// SPDX-License-Identifier: GPL-3.0-or-later
//! CardDAV server. Per user:
//! * `/carddav/colleagues/` – internal phone book, read-only
//! * `/carddav/contacts/`   – personal contacts, read-write (with an app password)
//! * `/carddav/services/`   – callable function numbers (emergency numbers, feature codes without
//!                            arguments, call groups, voice menus, company apps), read-only
//! * `/carddav/shared/`     – shared phone book (PBX_SHARED_CONTACTS=read|write), read-write with an app
//!                            password when PBX_SHARED_CONTACTS=write; deletes go to the bin (contacts.rs)
//! * `/carddav/book/`       – all of them combined, read-only, not listed; Linphone's remote contact directory
//!
//! Login: a device's SIP credentials (Digest, same realm as SIP, so Linphone reuses its SIP auth_info;
//! read-only), or an app password (login = `dav<user id>`, created under "Phone book"; read-write).
//! Basic is accepted too (HTTPS only).
//!
//! Supported: OPTIONS, PROPFIND (principal/home/book discovery, Depth 0/1, getctag), REPORT
//! (addressbook-query with text-match filters, addressbook-multiget), GET/HEAD, and PUT/DELETE
//! with If-Match/If-None-Match on personal contacts. Cards written by clients are stored verbatim.

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use sha2::Sha256;
use sqlx::FromRow;

use crate::{
    AppState,
    i18n::{Lang, Tr},
};

const BASE: &str = "/carddav/";
const BOOK: &str = "/carddav/book/";
const COLLEAGUES: &str = "/carddav/colleagues/";
const CONTACTS: &str = "/carddav/contacts/";
const SERVICES: &str = "/carddav/services/";
const SHARED: &str = "/carddav/shared/";

/// Writable book: the person's own contacts or the shared phone book.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Owner {
    Personal(i32),
    Shared,
}

impl Owner {
    /// SQL condition on contacts for this book; `$1` is the user id (ignored for the shared book)
    fn cond(self) -> &'static str {
        match self {
            Owner::Personal(_) => "user_id = $1",
            Owner::Shared => "user_id IS NULL AND deleted_at IS NULL AND $1 IS NOT NULL",
        }
    }
    fn uid(self, me: i32) -> i32 {
        match self {
            Owner::Personal(u) => u,
            Owner::Shared => me,
        }
    }
}
/// Digest nonces are valid this long (seconds)
const NONCE_TTL: i64 = 300;
/// Limits for client-written cards
const MAX_CARD: usize = 512 * 1024;
const MAX_BOOK_BYTES: i64 = 25 * 1024 * 1024;
pub const MAX_CONTACTS: i64 = 2000;
pub const MAX_TOKENS: i64 = 10;

// --- Authentication --------------------------------------------------------------------------

pub fn md5_hex(s: &str) -> String {
    hex::encode(Md5::digest(s.as_bytes()))
}

/// Stateless nonce: "<unix time>:<hmac(time)>"
fn nonce(app: &AppState, ts: i64) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(app.cookie_key.signing()).expect("hmac key");
    mac.update(format!("carddav-nonce:{ts}").as_bytes());
    format!("{ts}:{}", hex::encode(&mac.finalize().into_bytes()[..16]))
}

fn nonce_valid(app: &AppState, n: &str) -> bool {
    let Some((ts, _)) = n.split_once(':') else { return false };
    let Ok(ts) = ts.parse::<i64>() else { return false };
    let age = chrono::Utc::now().timestamp() - ts;
    (0..=NONCE_TTL).contains(&age) && nonce(app, ts) == n
}

fn unauthorized(app: &AppState) -> Response {
    let n = nonce(app, chrono::Utc::now().timestamp());
    let realm = &app.cfg.sip_realm;
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("WWW-Authenticate", format!("Digest realm=\"{realm}\", nonce=\"{n}\", qop=\"auth\", algorithm=MD5"))
        .header("WWW-Authenticate", format!("Basic realm=\"{realm}\""))
        .body(Body::empty())
        .unwrap()
}

/// Parses `Digest a="b", c=d` into pairs.
fn digest_params(s: &str) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    let mut rest = s;
    while !rest.is_empty() {
        rest = rest.trim_start_matches([',', ' ']);
        let Some((k, v)) = rest.split_once('=') else { break };
        let k = k.trim().to_lowercase();
        let (val, next) = if let Some(v) = v.strip_prefix('"') {
            match v.find('"') {
                Some(i) => (&v[..i], &v[i + 1..]),
                None => (v, ""),
            }
        } else {
            match v.find(',') {
                Some(i) => (&v[..i], &v[i..]),
                None => (v, ""),
            }
        };
        m.insert(k, val.trim().to_string());
        rest = next;
    }
    m
}

/// The logged-in user and whether the login may change personal contacts.
struct Principal {
    uid: i32,
    write: bool,
    /// phone permission (users.phone_allowed): required for the shared phone book
    phone: bool,
}

/// A credential that may match a login name: (user id, ha1, app password id).
type Cred = (i32, String, Option<i32>);

/// Devices log in with their SIP username, app passwords with "dav<user id>" (stable, unlike the
/// extension, which is part of the stored HA1).
async fn credentials(app: &AppState, user: &str) -> Vec<Cred> {
    let dev: Option<(i32, String)> = sqlx::query_as("SELECT user_id, ha1 FROM active_devices WHERE sip_username = $1")
        .bind(user)
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten();
    if let Some((uid, ha1)) = dev {
        return vec![(uid, ha1, None)];
    }
    let Some(uid) = user.strip_prefix("dav").filter(|r| !r.is_empty() && r.len() <= 9).and_then(|r| r.parse::<i32>().ok()) else {
        return vec![];
    };
    sqlx::query_as::<_, (i32, String, i32)>(
        "SELECT t.user_id, t.ha1, t.id FROM dav_tokens t JOIN users u ON u.id = t.user_id
         WHERE u.id = $1 AND u.deleted_at IS NULL AND u.enabled",
    )
    .bind(uid)
    .fetch_all(&app.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(u, h, id)| (u, h, Some(id)))
    .collect()
}

async fn authenticate(app: &AppState, method: &Method, headers: &HeaderMap) -> Option<Principal> {
    let auth = headers.get("authorization")?.to_str().ok()?;
    let found = if let Some(b) = auth.strip_prefix("Basic ") {
        let raw = String::from_utf8(B64.decode(b.trim()).ok()?).ok()?;
        let (user, pass) = raw.split_once(':')?;
        let ha1 = md5_hex(&format!("{user}:{}:{pass}", app.cfg.sip_realm));
        credentials(app, user).await.into_iter().find(|c| c.1 == ha1)
    } else {
        let p = digest_params(auth.strip_prefix("Digest ")?);
        let (user, n, uri, resp) = (p.get("username")?, p.get("nonce")?, p.get("uri")?, p.get("response")?);
        let reject = |why: &str| {
            let keys: Vec<String> =
                p.iter().filter(|(k, _)| matches!(k.as_str(), "username" | "realm" | "uri" | "algorithm" | "qop")).map(|(k, v)| format!("{k}={v}")).collect();
            tracing::info!("carddav digest rejected ({why}): {}", keys.join(" "));
        };
        if p.get("realm") != Some(&app.cfg.sip_realm) || !nonce_valid(app, n) {
            reject("realm/nonce");
            return None;
        }
        let creds = credentials(app, user).await;
        if creds.is_empty() {
            reject("unknown user");
            return None;
        }
        let ha2 = md5_hex(&format!("{}:{uri}", method.as_str()));
        let (nc, cnonce) = (p.get("nc").cloned().unwrap_or_default(), p.get("cnonce").cloned().unwrap_or_default());
        let qop_auth = p.get("qop").map(String::as_str) == Some("auth");
        let hit = creds.into_iter().find(|(_, ha1, _)| {
            let expected =
                if qop_auth { md5_hex(&format!("{ha1}:{n}:{nc}:{cnonce}:auth:{ha2}")) } else { md5_hex(&format!("{ha1}:{n}:{ha2}")) };
            &expected == resp
        });
        if hit.is_none() {
            reject("response mismatch");
        }
        hit
    };
    let (uid, _, token) = found?;
    if let Some(id) = token {
        let _ = sqlx::query("UPDATE dav_tokens SET last_used_at = now() WHERE id = $1 AND (last_used_at IS NULL OR last_used_at < now() - interval '5 minutes')")
            .bind(id)
            .execute(&app.db)
            .await;
    }
    let phone: bool = sqlx::query_scalar("SELECT phone_allowed AND enabled FROM users WHERE id = $1 AND deleted_at IS NULL")
        .bind(uid)
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten()
        .unwrap_or(false);
    Some(Principal { uid, write: token.is_some(), phone })
}

// --- vCards -------------------------------------------------------------------------------------

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace(',', "\\,").replace(';', "\\;").replace('\n', "\\n")
}

pub(crate) fn unesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Splits a structured value at unescaped ';'.
fn components(v: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut it = v.chars();
    while let Some(c) = it.next() {
        match c {
            '\\' => {
                cur.push(c);
                if let Some(n) = it.next() {
                    cur.push(n);
                }
            }
            ';' => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|c| unesc(&c)).collect()
}

/// Unfolded content lines.
pub(crate) fn unfold(card: &str) -> Vec<String> {
    let mut lines: Vec<String> = vec![];
    for raw in card.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if (raw.starts_with(' ') || raw.starts_with('\t')) && !lines.is_empty() {
            lines.last_mut().unwrap().push_str(&raw[1..]);
        } else if !raw.is_empty() {
            lines.push(raw.to_string());
        }
    }
    lines
}

/// (property name without group, upper-case; parameters, upper-case; raw value)
pub(crate) fn split_line(line: &str) -> Option<(String, String, &str)> {
    // the value starts at the first ':' outside double quotes
    let mut quoted = false;
    let colon = line.char_indices().find(|&(_, c)| {
        if c == '"' {
            quoted = !quoted;
        }
        c == ':' && !quoted
    })?.0;
    let head = &line[..colon];
    let (name, params) = head.split_once(';').unwrap_or((head, ""));
    let name = name.rsplit('.').next().unwrap_or(name).to_ascii_uppercase();
    Some((name, params.to_ascii_uppercase(), &line[colon + 1..]))
}

/// Fields of a card the phone system uses (phone book, caller names, search).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Fields {
    pub name: String,
    pub organization: Option<String>,
    pub phone: Option<String>,
    pub mobile: Option<String>,
    pub email: Option<String>,
    pub note: Option<String>,
}

fn clip(s: &str, max: usize) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.chars().take(max).collect())
}

/// Extracts the fields from a vCard (3.0 or 4.0). None if it is not a single vCard.
pub fn parse_vcard(card: &str) -> Option<Fields> {
    let lines = unfold(card);
    let first = lines.first()?.to_ascii_uppercase();
    let last = lines.last()?.to_ascii_uppercase();
    if first != "BEGIN:VCARD" || last != "END:VCARD" || lines.iter().filter(|l| l.eq_ignore_ascii_case("BEGIN:VCARD")).count() != 1 {
        return None;
    }
    let (mut fname, mut n, mut f) = (None, None, Fields::default());
    for l in &lines {
        let Some((name, params, value)) = split_line(l) else { continue };
        match name.as_str() {
            "FN" if fname.is_none() => fname = clip(&unesc(value), 80),
            "N" if n.is_none() => {
                let c = components(value);
                let given = c.get(1).map(String::as_str).unwrap_or("");
                let family = c.first().map(String::as_str).unwrap_or("");
                n = clip(&format!("{given} {family}"), 80);
            }
            "ORG" if f.organization.is_none() => f.organization = clip(&components(value).join(" "), 80),
            "EMAIL" if f.email.is_none() => f.email = clip(&unesc(value).trim_start_matches("mailto:"), 120),
            "NOTE" if f.note.is_none() => f.note = clip(&unesc(value), 500),
            "TEL" => {
                let v = clip(unesc(value).trim_start_matches("tel:"), 40);
                if params.contains("CELL") {
                    if f.mobile.is_none() {
                        f.mobile = v;
                    }
                } else if f.phone.is_none() && !params.contains("FAX") {
                    f.phone = v;
                }
            }
            _ => {}
        }
    }
    f.name = fname
        .or(n)
        .or(f.organization.clone())
        .or(f.phone.clone())
        .or(f.mobile.clone())
        .or(f.email.clone())
        .unwrap_or_else(|| "?".into());
    Some(f)
}

/// Card for a contact created in the web UI.
pub fn build_vcard(uid: &str, f: &Fields, family_given: bool) -> String {
    let (given, family) = match f.name.trim().rsplit_once(' ') {
        Some((g, fam)) if family_given => (g, fam),
        _ => ("", f.name.as_str()),
    };
    let mut v = format!("BEGIN:VCARD\r\nVERSION:3.0\r\nUID:{uid}\r\nFN:{}\r\nN:{};{};;;\r\n", esc(&f.name), esc(family), esc(given));
    if let Some(o) = &f.organization {
        v += &format!("ORG:{}\r\n", esc(o));
    }
    if let Some(p) = &f.phone {
        v += &format!("TEL;TYPE=VOICE:{}\r\n", esc(p));
    }
    if let Some(m) = &f.mobile {
        v += &format!("TEL;TYPE=CELL:{}\r\n", esc(m));
    }
    if let Some(e) = &f.email {
        v += &format!("EMAIL:{}\r\n", esc(e));
    }
    if let Some(n) = &f.note {
        v += &format!("NOTE:{}\r\n", esc(n));
    }
    v + "END:VCARD\r\n"
}

fn digits(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// Applies a web UI edit (old → new fields) to a client-written card; everything the UI does not
/// show (addresses, photos, further numbers, ...) is kept.
pub fn patch_vcard(card: &str, old: &Fields, new: &Fields) -> String {
    let mut out: Vec<String> = vec![];
    for l in unfold(card) {
        let Some((name, params, value)) = split_line(&l) else {
            out.push(l);
            continue;
        };
        let drop = match name.as_str() {
            "END" => continue,
            "FN" | "N" => old.name != new.name,
            "ORG" => old.organization != new.organization,
            "NOTE" => old.note != new.note,
            "EMAIL" => old.email != new.email && old.email.as_deref() == Some(unesc(value).trim_start_matches("mailto:")),
            "TEL" => {
                let d = digits(&unesc(value));
                let cell = params.contains("CELL");
                (!cell && old.phone != new.phone && old.phone.as_deref().map(digits) == Some(d.clone()))
                    || (cell && old.mobile != new.mobile && old.mobile.as_deref().map(digits) == Some(d))
            }
            _ => false,
        };
        if !drop {
            out.push(l);
        }
    }
    if old.name != new.name {
        let (given, family) = match new.name.trim().rsplit_once(' ') {
            Some((g, f)) if new.organization.as_deref() != Some(new.name.as_str()) => (g, f),
            _ => ("", new.name.as_str()),
        };
        out.push(format!("FN:{}", esc(&new.name)));
        out.push(format!("N:{};{};;;", esc(family), esc(given)));
    }
    if old.organization != new.organization {
        if let Some(o) = &new.organization {
            out.push(format!("ORG:{}", esc(o)));
        }
    }
    if old.note != new.note {
        if let Some(n) = &new.note {
            out.push(format!("NOTE:{}", esc(n)));
        }
    }
    if old.email != new.email {
        if let Some(e) = &new.email {
            out.push(format!("EMAIL:{}", esc(e)));
        }
    }
    if old.phone != new.phone {
        if let Some(p) = &new.phone {
            out.push(format!("TEL;TYPE=VOICE:{}", esc(p)));
        }
    }
    if old.mobile != new.mobile {
        if let Some(m) = &new.mobile {
            out.push(format!("TEL;TYPE=CELL:{}", esc(m)));
        }
    }
    out.push("END:VCARD".into());
    out.join("\r\n") + "\r\n"
}

// --- Data -------------------------------------------------------------------------------------

struct Card {
    /// resource name inside its collection, e.g. "u2.vcf"
    res: String,
    vcard: String,
    /// lower-case search text (name, company, numbers, e-mail)
    hay: String,
}

impl Card {
    fn etag(&self) -> String {
        format!("\"{}\"", &md5_hex(&self.vcard)[..16])
    }
    fn matches(&self, needles: &[String]) -> bool {
        if needles.is_empty() {
            return true;
        }
        let dig = digits(&self.hay);
        needles.iter().any(|n| {
            let n = n.to_lowercase();
            let nd = digits(&n);
            self.hay.contains(&n) || (!nd.is_empty() && nd.len() == n.chars().filter(|c| !c.is_whitespace()).count() && dig.contains(&nd))
        })
    }
}

fn hay(parts: &[&Option<String>]) -> String {
    parts.iter().filter_map(|p| p.as_deref()).collect::<Vec<_>>().join(" ").to_lowercase()
}

#[derive(FromRow)]
struct ColleagueRow {
    id: i32,
    name: String,
    email: Option<String>,
    internal: String,
}

async fn colleagues(app: &AppState) -> anyhow::Result<Vec<Card>> {
    let rows: Vec<ColleagueRow> = sqlx::query_as(
        "SELECT u.id, u.display_name AS name, u.email, u.extension::text AS internal
         FROM users u WHERE u.deleted_at IS NULL AND u.enabled AND (u.in_phonebook OR $1) ORDER BY u.display_name",
    )
    .bind(app.cfg.phonebook_enforced)
    .fetch_all(&app.db)
    .await?;
    let realm = &app.cfg.sip_realm;
    Ok(rows
        .into_iter()
        .map(|r| {
            let (given, family) = r.name.trim().rsplit_once(' ').unwrap_or(("", r.name.as_str()));
            let mut v = format!(
                "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:u{}\r\nFN:{}\r\nN:{};{};;;\r\nTEL;TYPE=WORK:{i}\r\nIMPP:sip:{i}@{realm}\r\n",
                r.id,
                esc(&r.name),
                esc(family),
                esc(given),
                i = r.internal
            );
            if let Some(e) = &r.email {
                v += &format!("EMAIL:{}\r\n", esc(e));
            }
            v += "END:VCARD\r\n";
            Card { res: format!("u{}.vcf", r.id), hay: hay(&[&Some(r.name.clone()), &Some(r.internal.clone()), &r.email]), vcard: v }
        })
        .collect())
}

/// Card of a "Services" entry, grouped by ORG/CATEGORIES (company name "Dienste"/"Services").
/// No `X-` properties: Linphone parses with belcard's vCard 3.0 grammar, whose `property` rule has
/// no X-PROPERTY alternative, and a card with one unknown line is dropped silently (this is what
/// `X-ABShowAs` did). CATEGORIES is a text-list there, so `\;` would break it as well.
/// IMPP carries the SIP address ('#' percent-encoded).
fn service_vcard(s: &crate::help::Service, org: &str, realm: &str) -> String {
    let mut v = format!(
        "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:{}\r\nFN:{}\r\nN:{};;;;\r\nORG:{}\r\nCATEGORIES:{}\r\nTITLE:{}\r\n\
         TEL;TYPE=WORK:{}\r\nIMPP:sip:{}@{realm}\r\n",
        s.uid,
        esc(&s.name),
        esc(&s.name),
        esc(org),
        esc(org).replace("\\;", " "),
        esc(&s.kind),
        esc(&s.number),
        s.number.replace('#', "%23"),
    );
    if let Some(n) = &s.note {
        v += &format!("NOTE:{}\r\n", esc(n));
    }
    v + "END:VCARD\r\n"
}

fn service_cards(list: &[crate::help::Service], org: &str, realm: &str) -> Vec<Card> {
    list.iter()
        .map(|s| Card {
            res: format!("{}.vcf", s.uid),
            vcard: service_vcard(s, org, realm),
            hay: format!("{} {} {} {}", s.name, s.number, s.kind, org).to_lowercase(),
        })
        .collect()
}

async fn services(app: &AppState, t: Tr) -> anyhow::Result<Vec<Card>> {
    Ok(service_cards(&crate::help::services(app, t).await?, t.s("Services"), &app.cfg.sip_realm))
}

/// UI language of the user (web setting), else the client's Accept-Language.
async fn user_tr(app: &AppState, uid: i32, headers: &HeaderMap) -> Tr {
    let l: Option<Option<String>> = sqlx::query_scalar("SELECT ui_language FROM users WHERE id = $1").bind(uid).fetch_optional(&app.db).await.ok().flatten();
    Tr(l.flatten().as_deref().and_then(Lang::parse).unwrap_or_else(|| Lang::from_headers(headers)))
}

#[derive(FromRow, Clone)]
struct ContactRow {
    id: i32,
    name: String,
    organization: Option<String>,
    phone: Option<String>,
    mobile: Option<String>,
    email: Option<String>,
    note: Option<String>,
    vcard: Option<String>,
    dav_name: Option<String>,
}

impl ContactRow {
    fn fields(&self) -> Fields {
        Fields {
            name: self.name.clone(),
            organization: self.organization.clone(),
            phone: self.phone.clone(),
            mobile: self.mobile.clone(),
            email: self.email.clone(),
            note: self.note.clone(),
        }
    }
    fn card(self) -> Card {
        let f = self.fields();
        let res = self.dav_name.clone().unwrap_or_else(|| format!("c{}.vcf", self.id));
        let vcard = self.vcard.clone().unwrap_or_else(|| build_vcard(&format!("c{}", self.id), &f, false));
        Card { res, vcard, hay: hay(&[&Some(f.name.clone()), &f.organization, &f.phone, &f.mobile, &f.email]) }
    }
}

const CONTACT_COLS: &str = "id, name, organization, phone, mobile, email, note, vcard, dav_name";

async fn personal(app: &AppState, uid: i32) -> anyhow::Result<Vec<Card>> {
    owned(app, Owner::Personal(uid)).await
}

async fn owned(app: &AppState, owner: Owner) -> anyhow::Result<Vec<Card>> {
    let rows: Vec<ContactRow> = sqlx::query_as(&format!("SELECT {CONTACT_COLS} FROM contacts WHERE {} ORDER BY lower(name)", owner.cond()))
        .bind(owner.uid(0))
        .fetch_all(&app.db)
        .await?;
    Ok(rows.into_iter().map(ContactRow::card).collect())
}

/// The shared phone book (none while PBX_SHARED_CONTACTS=off or without phone permission)
async fn shared(app: &AppState, me: &Principal) -> anyhow::Result<Vec<Card>> {
    if !app.cfg.shared_contacts.on() || !me.phone {
        return Ok(vec![]);
    }
    owned(app, Owner::Shared).await
}

async fn contact_by_res(app: &AppState, owner: Owner, res: &str) -> anyhow::Result<Option<ContactRow>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {CONTACT_COLS} FROM contacts WHERE {} AND (dav_name = $2 OR (dav_name IS NULL AND 'c' || id || '.vcf' = $2))",
        owner.cond()
    ))
    .bind(owner.uid(0))
    .bind(res)
    .fetch_optional(&app.db)
    .await?)
}

/// Phone number as stored: internal numbers as digits, external ones as written plus E.164 if parseable.
pub fn store_number(v: &Option<String>) -> (Option<String>, Option<String>) {
    let Some(v) = v else { return (None, None) };
    let d = digits(v);
    if !v.starts_with('+') && !d.starts_with('0') && (2..=7).contains(&d.len()) {
        return (Some(d), None);
    }
    (Some(v.clone()), crate::admin::to_e164(v))
}

// --- XML ----------------------------------------------------------------------------------------

fn xml_esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn multistatus(responses: &str) -> Response {
    Response::builder()
        .status(207)
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(Body::from(format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:carddav\" \
             xmlns:cs=\"http://calendarserver.org/ns/\">{responses}</d:multistatus>"
        )))
        .unwrap()
}

/// A collection: the home (book = None) or an address book with its ctag.
fn collection(href: &str, name: &str, book: Option<&str>, write: bool) -> String {
    let privs = if write { "<d:privilege><d:read/></d:privilege><d:privilege><d:write/></d:privilege>" } else { "<d:privilege><d:read/></d:privilege>" };
    let book_props = match book {
        Some(ctag) => format!(
            "<cs:getctag>{ctag}</cs:getctag>\
             <d:supported-report-set><d:supported-report><d:report><c:addressbook-query/></d:report></d:supported-report>\
             <d:supported-report><d:report><c:addressbook-multiget/></d:report></d:supported-report></d:supported-report-set>\
             <c:supported-address-data><c:address-data-type content-type=\"text/vcard\" version=\"3.0\"/></c:supported-address-data>\
             <c:max-resource-size>{MAX_CARD}</c:max-resource-size>"
        ),
        None => String::new(),
    };
    // Asked on an address book itself, principal and home point to that book: Linphone's discovery
    // (liblinphone CardDAVContext) always takes the FIRST address book of the home set and overwrites
    // the configured URL with it, so a book URL must lead back to exactly that book. Clients that
    // discover from the root (/carddav/, well-known) still get the home with all books.
    let home = if book.is_some() { href } else { BASE };
    format!(
        "<d:response><d:href>{href}</d:href><d:propstat><d:prop>\
         <d:resourcetype><d:collection/>{}</d:resourcetype><d:displayname>{}</d:displayname>\
         <d:current-user-principal><d:href>{home}</d:href></d:current-user-principal>\
         <d:principal-URL><d:href>{home}</d:href></d:principal-URL>\
         <c:addressbook-home-set><d:href>{home}</d:href></c:addressbook-home-set>\
         <d:current-user-privilege-set>{privs}</d:current-user-privilege-set>{book_props}\
         </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
        if book.is_some() { "<c:addressbook/>" } else { "<d:principal/>" },
        xml_esc(name)
    )
}

/// Collection tag: changes with any card. `epoch` (setting carddav_epoch) changes all of them at
/// once: clients that only compare the ctag (Linphone) then fetch the etags again and load what is
/// missing ("force resync").
fn ctag(cards: &[Card], epoch: &str) -> String {
    let all: String = cards.iter().map(|c| format!("{}{}", c.res, c.etag())).collect();
    md5_hex(&format!("{epoch}{all}"))[..16].to_string()
}

async fn epoch(app: &AppState) -> String {
    sqlx::query_scalar("SELECT value FROM settings WHERE key = 'carddav_epoch'").fetch_optional(&app.db).await.ok().flatten().unwrap_or_default()
}

fn card_response(coll: &str, c: &Card, with_data: bool) -> String {
    format!(
        "<d:response><d:href>{coll}{}</d:href><d:propstat><d:prop><d:getetag>{}</d:getetag>\
         <d:getcontenttype>text/vcard; charset=utf-8</d:getcontenttype><d:resourcetype/>{}</d:prop>\
         <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
        xml_esc(&c.res),
        xml_esc(&c.etag()),
        if with_data { format!("<c:address-data>{}</c:address-data>", xml_esc(&c.vcard)) } else { String::new() }
    )
}

/// Values of all <...text-match ...>value</...> elements (namespace prefixes vary by client).
fn text_matches(body: &str) -> Vec<String> {
    let mut out = vec![];
    let mut rest = body;
    while let Some(i) = rest.find("text-match") {
        rest = &rest[i..];
        let Some(gt) = rest.find('>') else { break };
        if rest[..gt].ends_with('/') {
            rest = &rest[gt..];
            continue;
        }
        let after = &rest[gt + 1..];
        let Some(lt) = after.find('<') else { break };
        let v = after[..lt].trim();
        if !v.is_empty() {
            out.push(v.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">"));
        }
        rest = &after[lt..];
    }
    out
}

/// hrefs of an addressbook-multiget (percent-decoded). Matches `<href>`, `<d:href>` and opening
/// tags with attributes such as iOS' `<A:href xmlns:A="DAV:">`.
fn hrefs(body: &str) -> Vec<String> {
    let mut out = vec![];
    let mut rest = body;
    while let Some(i) = rest.find('<') {
        let tag = &rest[i + 1..];
        let Some(gt) = tag.find('>') else { break };
        let name = tag[..gt].split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
        let local = name.rsplit(':').next().unwrap_or("");
        rest = &tag[gt + 1..];
        if local != "href" || tag.starts_with('/') || tag[..gt].ends_with('/') {
            continue;
        }
        let Some(lt) = rest.find('<') else { break };
        let h = rest[..lt].trim().replace("&amp;", "&");
        out.push(percent_encoding::percent_decode_str(&h).decode_utf8_lossy().into_owned());
        rest = &rest[lt..];
    }
    out
}

/// Resource names a client may create: no path separators, no names of generated cards.
fn valid_res(res: &str) -> bool {
    !res.is_empty()
        && res.len() <= 200
        && res.chars().all(|c| c.is_ascii_alphanumeric() || "._-@+=~".contains(c))
        && !(res.starts_with('u') && res.ends_with(".vcf") && res[1..res.len() - 4].chars().all(|c| c.is_ascii_digit()))
        && !res.starts_with("svc-")
        && !res.starts_with('.')
        // c<id>.vcf names cards without a client name (web UI, restored entries): a client must not
        // take such a name, it would collide with another contact
        && !(res.starts_with('c') && res.ends_with(".vcf") && res.len() > 5 && res[1..res.len() - 4].chars().all(|c| c.is_ascii_digit()))
}

// --- Handler ------------------------------------------------------------------------------------

/// Single entry point for /carddav and /.well-known/carddav (all methods).
pub async fn handle(State(app): State<AppState>, method: Method, uri: Uri, headers: HeaderMap, body: String) -> Response {
    let ua = headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("-").to_string();
    let req_body: String = body.chars().take(600).collect();
    let (m, p) = (method.clone(), uri.path().to_string());
    let resp = match handle_inner(&app, method, uri, &headers, body).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("carddav {m} {p}: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    };
    tracing::debug!("carddav {m} {p} -> {} ua={ua:?} body={req_body:?}", resp.status());
    resp
}

fn status(s: StatusCode) -> anyhow::Result<Response> {
    Ok(s.into_response())
}

async fn handle_inner(app: &AppState, method: Method, uri: Uri, headers: &HeaderMap, body: String) -> anyhow::Result<Response> {
    let path = percent_encoding::percent_decode_str(uri.path()).decode_utf8_lossy().into_owned();
    if path.starts_with("/.well-known/carddav") {
        return Ok(Response::builder().status(StatusCode::MOVED_PERMANENTLY).header("Location", BASE).body(Body::empty())?);
    }
    if method == Method::OPTIONS {
        return Ok(Response::builder()
            .header("Allow", "OPTIONS, GET, HEAD, PROPFIND, REPORT, PUT, DELETE")
            .header("DAV", "1, 3, addressbook")
            .body(Body::empty())?);
    }
    let Some(me) = authenticate(app, &method, headers).await else { return Ok(unauthorized(app)) };
    let path = if path == "/carddav" { BASE.to_string() } else { path };
    let t = user_tr(app, me.uid, headers).await;
    // which collection and resource
    // external phone books (PBX_EXTERNAL_BOOKS): /carddav/ext-<id>/, read-only
    let ext_paths: Vec<String> = app.cfg.external_books.iter().map(|b| format!("/carddav/ext-{}/", b.id)).collect();
    let (coll, res): (Option<String>, Option<String>) = [BOOK, COLLEAGUES, CONTACTS, SERVICES, SHARED]
        .iter()
        .map(|c| c.to_string())
        .chain(ext_paths.iter().cloned())
        .find_map(|c| {
            if path == c || path == c.trim_end_matches('/') {
                Some((c, None))
            } else {
                let r = path.strip_prefix(c.as_str()).map(str::to_string);
                r.map(|r| (c, Some(r)))
            }
        })
        .map(|(c, r)| (Some(c), r))
        .unwrap_or((None, None));
    let depth = headers.get("depth").and_then(|d| d.to_str().ok()).unwrap_or("0").to_string();

    if method.as_str() == "PROPFIND" && path == BASE {
        let mut r = collection(BASE, "Telefonie", None, false);
        if depth != "0" {
            let ep = epoch(app).await;
            r += &collection(COLLEAGUES, &app.cfg.site_title, Some(&ctag(&colleagues(app).await?, &ep)), false);
            r += &collection(CONTACTS, t.s("My contacts"), Some(&ctag(&personal(app, me.uid).await?, &ep)), me.write);
            r += &collection(SERVICES, t.s("Services"), Some(&ctag(&services(app, t).await?, &ep)), false);
            if app.cfg.shared_contacts.on() && me.phone {
                r += &collection(SHARED, t.s("Shared phone book"), Some(&ctag(&shared(app, &me).await?, &ep)), shared_write(app, &me));
            }
            if me.phone {
                for b in &app.cfg.external_books {
                    let path = format!("/carddav/ext-{}/", b.id);
                    r += &collection(&path, &b.name, Some(&ctag(&external(app, &b.id).await?, &ep)), false);
                }
            }
        }
        return Ok(multistatus(&r));
    }
    let Some(coll) = coll else { return status(StatusCode::NOT_FOUND) };
    let coll = coll.as_str();
    let ext_book = app.cfg.external_books.iter().find(|b| coll == format!("/carddav/ext-{}/", b.id));
    if (coll == SHARED && !(app.cfg.shared_contacts.on() && me.phone)) || (ext_book.is_some() && !me.phone) {
        return status(StatusCode::NOT_FOUND);
    }

    match method.as_str() {
        "PUT" if coll == CONTACTS => return put_contact(app, &me, Owner::Personal(me.uid), res.as_deref(), headers, body).await,
        "DELETE" if coll == CONTACTS => return delete_contact(app, &me, Owner::Personal(me.uid), res.as_deref(), headers).await,
        "PUT" if coll == SHARED => return put_contact(app, &me, Owner::Shared, res.as_deref(), headers, body).await,
        "DELETE" if coll == SHARED => return delete_contact(app, &me, Owner::Shared, res.as_deref(), headers).await,
        "PUT" | "DELETE" | "PROPPATCH" | "MKCOL" => return status(StatusCode::FORBIDDEN),
        _ => {}
    }

    let cards = match (coll, ext_book) {
        (_, Some(b)) => external(app, &b.id).await?,
        (COLLEAGUES, _) => colleagues(app).await?,
        (CONTACTS, _) => personal(app, me.uid).await?,
        (SERVICES, _) => services(app, t).await?,
        (SHARED, _) => shared(app, &me).await?,
        _ => {
            let mut c = colleagues(app).await?;
            c.extend(personal(app, me.uid).await?);
            c.extend(shared(app, &me).await?);
            if me.phone {
                for b in &app.cfg.external_books {
                    c.extend(external_prefixed(app, &b.id).await?);
                }
            }
            c.extend(services(app, t).await?);
            c
        }
    };
    let (name, write) = match (coll, ext_book) {
        (_, Some(b)) => (b.name.as_str(), false),
        (COLLEAGUES, _) => (app.cfg.site_title.as_str(), false),
        (CONTACTS, _) => (t.s("My contacts"), me.write),
        (SERVICES, _) => (t.s("Services"), false),
        (SHARED, _) => (t.s("Shared phone book"), shared_write(app, &me)),
        _ => (app.cfg.site_title.as_str(), false),
    };
    match (method.as_str(), res.as_deref()) {
        ("PROPFIND", None) => {
            let mut r = collection(coll, name, Some(&ctag(&cards, &epoch(app).await)), write);
            if depth != "0" {
                for c in &cards {
                    r += &card_response(coll, c, false);
                }
            }
            Ok(multistatus(&r))
        }
        ("PROPFIND", Some(res)) => match cards.iter().find(|c| c.res == res) {
            Some(c) => Ok(multistatus(&card_response(coll, c, false))),
            None => status(StatusCode::NOT_FOUND),
        },
        ("REPORT", None) => {
            if body.contains("addressbook-multiget") {
                let wanted = hrefs(&body);
                let r: String = cards
                    .iter()
                    .filter(|c| wanted.iter().any(|h| h.rsplit('/').next() == Some(c.res.as_str())))
                    .map(|c| card_response(coll, c, true))
                    .collect();
                Ok(multistatus(&r))
            } else {
                // addressbook-query: text-match filters (anyof), at most 100 results
                let needles = text_matches(&body);
                let r: String = cards.iter().filter(|c| c.matches(&needles)).take(100).map(|c| card_response(coll, c, true)).collect();
                Ok(multistatus(&r))
            }
        }
        ("GET" | "HEAD", Some(res)) => match cards.iter().find(|c| c.res == res) {
            Some(c) => Ok(Response::builder()
                .header("Content-Type", "text/vcard; charset=utf-8")
                .header("ETag", c.etag())
                .body(Body::from(if method == Method::HEAD { String::new() } else { c.vcard.clone() }))?),
            None => status(StatusCode::NOT_FOUND),
        },
        ("GET" | "HEAD", None) => status(StatusCode::METHOD_NOT_ALLOWED),
        ("REPORT", Some(_)) => status(StatusCode::NOT_FOUND),
        _ => status(StatusCode::METHOD_NOT_ALLOWED),
    }
}

/// If-Match / If-None-Match against the current etag (None = resource does not exist).
fn precondition_ok(headers: &HeaderMap, current: Option<&str>) -> bool {
    let h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).map(str::trim);
    if let Some(m) = h("if-match") {
        match current {
            None => return false,
            Some(cur) => {
                if m != "*" && !m.split(',').any(|t| t.trim().trim_start_matches("W/") == cur) {
                    return false;
                }
            }
        }
    }
    if let Some(m) = h("if-none-match") {
        if let Some(cur) = current {
            if m == "*" || m.split(',').any(|t| t.trim().trim_start_matches("W/") == cur) {
                return false;
            }
        }
    }
    true
}

/// May this login change the shared phone book over CardDAV: an app password, and everyone may edit
/// (PBX_SHARED_CONTACTS=write). With "read" only admins edit, in the web UI.
fn shared_write(app: &AppState, me: &Principal) -> bool {
    me.write && me.phone && app.cfg.shared_contacts == crate::config::SharedContacts::Write
}

async fn put_contact(app: &AppState, me: &Principal, owner: Owner, res: Option<&str>, headers: &HeaderMap, body: String) -> anyhow::Result<Response> {
    if !me.write || (owner == Owner::Shared && !shared_write(app, me)) {
        return status(StatusCode::FORBIDDEN);
    }
    let Some(res) = res.filter(|r| r.len() <= 200 && !r.is_empty()) else { return status(StatusCode::FORBIDDEN) };
    if body.len() > MAX_CARD {
        return status(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let Some(f) = parse_vcard(&body) else { return status(StatusCode::UNSUPPORTED_MEDIA_TYPE) };
    let existing = contact_by_res(app, owner, res).await?;
    // names of new cards are checked; an existing card keeps the name it has (also c<id>.vcf)
    if existing.is_none() && !valid_res(res) {
        return status(StatusCode::FORBIDDEN);
    }
    let cur_etag = existing.as_ref().map(|r| r.clone().card().etag());
    if !precondition_ok(headers, cur_etag.as_deref()) {
        return status(StatusCode::PRECONDITION_FAILED);
    }
    let (count, bytes): (i64, i64) =
        sqlx::query_as(&format!("SELECT count(*), COALESCE(sum(length(vcard)), 0)::bigint FROM contacts WHERE {}", owner.cond()))
            .bind(owner.uid(me.uid))
            .fetch_one(&app.db)
            .await?;
    let (max, max_bytes) = match owner {
        Owner::Personal(_) => (MAX_CONTACTS, MAX_BOOK_BYTES),
        Owner::Shared => (crate::contacts::MAX_SHARED, 4 * MAX_BOOK_BYTES),
    };
    let old_len = existing.as_ref().and_then(|r| r.vcard.as_ref()).map(|v| v.len() as i64).unwrap_or(0);
    if (existing.is_none() && count >= max) || bytes - old_len + body.len() as i64 > max_bytes {
        return status(StatusCode::INSUFFICIENT_STORAGE);
    }
    let (phone, phone_e164) = store_number(&f.phone);
    let (mobile, mobile_e164) = store_number(&f.mobile);
    let q = if existing.is_some() {
        "UPDATE contacts SET name = $3, organization = $4, phone = $5, phone_e164 = $6, mobile = $7, mobile_e164 = $8,
                email = $9, note = $10, vcard = $11, updated_at = now(), updated_by = $2
         WHERE id = $1 AND (user_id = $2 OR ($13 AND user_id IS NULL AND deleted_at IS NULL))"
    } else {
        "INSERT INTO contacts (user_id, name, organization, phone, phone_e164, mobile, mobile_e164, email, note, vcard, dav_name, created_by, updated_by)
         VALUES (CASE WHEN $13 THEN NULL ELSE $2 END, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $2, $2)"
    };
    let is_shared = owner == Owner::Shared;
    sqlx::query(q)
        .bind(existing.as_ref().map(|r| r.id).unwrap_or(0))
        .bind(me.uid)
        .bind(&f.name)
        .bind(&f.organization)
        .bind(&phone)
        .bind(&phone_e164)
        .bind(&mobile)
        .bind(&mobile_e164)
        .bind(&f.email)
        .bind(&f.note)
        .bind(&body)
        .bind(res)
        .bind(is_shared)
        .execute(&app.db)
        .await?;
    if is_shared {
        let action = if existing.is_some() { "contact.shared.update" } else { "contact.shared.create" };
        crate::db::audit(&app.db, &format!("dav:{}", me.uid), action, &format!("dav:{res}"), serde_json::json!({"name": f.name})).await?;
    }
    let etag = format!("\"{}\"", &md5_hex(&body)[..16]);
    Ok(Response::builder()
        .status(if existing.is_some() { StatusCode::NO_CONTENT } else { StatusCode::CREATED })
        .header("ETag", etag)
        .body(Body::empty())?)
}

async fn delete_contact(app: &AppState, me: &Principal, owner: Owner, res: Option<&str>, headers: &HeaderMap) -> anyhow::Result<Response> {
    if !me.write || (owner == Owner::Shared && !shared_write(app, me)) {
        return status(StatusCode::FORBIDDEN);
    }
    let Some(res) = res else { return status(StatusCode::FORBIDDEN) };
    let Some(row) = contact_by_res(app, owner, res).await? else { return status(StatusCode::NOT_FOUND) };
    let etag = row.clone().card().etag();
    if !precondition_ok(headers, Some(&etag)) {
        return status(StatusCode::PRECONDITION_FAILED);
    }
    match owner {
        Owner::Personal(uid) => {
            sqlx::query("DELETE FROM contacts WHERE id = $1 AND user_id = $2").bind(row.id).bind(uid).execute(&app.db).await?;
        }
        // shared entries go to the bin (restored by admins in the web UI)
        Owner::Shared => {
            sqlx::query("UPDATE contacts SET deleted_at = now(), deleted_by = $2 WHERE id = $1 AND user_id IS NULL AND deleted_at IS NULL")
                .bind(row.id)
                .bind(me.uid)
                .execute(&app.db)
                .await?;
            crate::db::audit(&app.db, &format!("dav:{}", me.uid), "contact.shared.delete", &format!("contact:{}", row.id), serde_json::json!({"name": row.name}))
                .await?;
        }
    }
    status(StatusCode::NO_CONTENT)
}

/// Cards of an external phone book (stored copies, read-only); in the combined book the names get the
/// prefix "ext-<id>-" (two books of the same software use the same names)
async fn external(app: &AppState, book: &str) -> anyhow::Result<Vec<Card>> {
    Ok(crate::extbooks::cards(app, book).await?.into_iter().map(|(res, vcard)| Card { hay: vcard.to_lowercase(), res, vcard }).collect())
}

async fn external_prefixed(app: &AppState, book: &str) -> anyhow::Result<Vec<Card>> {
    Ok(external(app, book).await?.into_iter().map(|c| Card { res: format!("ext-{book}-{}", c.res), ..c }).collect())
}

/// Fields and stored card of a contact (for edits in the web UI): the person's own (`Some(uid)`) or a
/// shared one (`None`).
pub async fn contact_card(db: &sqlx::PgPool, owner: Option<i32>, id: i32) -> anyhow::Result<Option<(Fields, Option<String>)>> {
    let row: Option<ContactRow> = sqlx::query_as(&format!(
        "SELECT {CONTACT_COLS} FROM contacts WHERE id = $1 AND (user_id = $2 OR ($2 IS NULL AND user_id IS NULL AND deleted_at IS NULL))"
    ))
    .bind(id)
    .bind(owner)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|r| (r.fields(), r.vcard)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiget_hrefs_with_attributes() {
        let ios = "<?xml version=\"1.0\"?><D:addressbook-multiget xmlns:D=\"urn:ietf:params:xml:ns:carddav\">\
            <A:prop xmlns:A=\"DAV:\"><A:getetag/><D:address-data/></A:prop>\
            <A:href xmlns:A=\"DAV:\">/carddav/colleagues/u5.vcf</A:href>\
            <A:href xmlns:A=\"DAV:\">/carddav/colleagues/a%20b.vcf</A:href></D:addressbook-multiget>";
        assert_eq!(hrefs(ios), vec!["/carddav/colleagues/u5.vcf", "/carddav/colleagues/a b.vcf"]);
        assert_eq!(hrefs("<d:href>/x/u1.vcf</d:href><href>/x/u2.vcf</href>"), vec!["/x/u1.vcf", "/x/u2.vcf"]);
    }

    #[test]
    fn parses_text_match_and_digest() {
        let q = r#"<C:addressbook-query xmlns:C="urn:ietf:params:xml:ns:carddav"><C:filter><C:prop-filter name="FN"><C:text-match collation="i;unicode-casemap" match-type="contains">Phil</C:text-match></C:prop-filter></C:filter></C:addressbook-query>"#;
        assert_eq!(text_matches(q), vec!["Phil".to_string()]);
        let p = digest_params(r#"username="abc", realm="voip.x", nonce="1:ff", uri="/carddav/book/", qop=auth, nc=00000001, cnonce="xy", response="00""#);
        assert_eq!(p.get("qop").unwrap(), "auth");
        assert_eq!(p.get("nc").unwrap(), "00000001");
        assert_eq!(p.get("uri").unwrap(), "/carddav/book/");
    }

    #[test]
    fn matches_digits_and_names() {
        let c = Card { res: "u1.vcf".into(), vcard: String::new(), hay: "ada lovelace 0176 1234 56".into() };
        assert!(c.matches(&["ada".into()]));
        assert!(c.matches(&["01761234".into()]));
        assert!(!c.matches(&["grace".into()]));
    }

    const IOS: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nPRODID:-//Apple Inc.//iPhone OS 18.0//EN\r\nN:Muster;Erika;;;\r\nFN:Erika Muster\r\n\
ORG:Beispiel GmbH;Vertrieb\r\nitem1.TEL;type=CELL;type=VOICE;type=pref:+49 176 1234567\r\nTEL;type=WORK;type=VOICE:040 123\r\n\x20456\r\nTEL;type=WORK;type=FAX:040 999\r\nEMAIL;type=INTERNET:erika@example.com\r\nADR;type=WORK:;;Hauptstr. 1;Hamburg;;20095;DE\r\n\
NOTE:Kunde seit 2020\\, wichtig\r\nPHOTO;ENCODING=b;TYPE=JPEG:AAAA\r\nUID:abc-123\r\nEND:VCARD\r\n";

    #[test]
    fn parses_client_card() {
        let f = parse_vcard(IOS).unwrap();
        assert_eq!(f.name, "Erika Muster");
        assert_eq!(f.organization.as_deref(), Some("Beispiel GmbH Vertrieb"));
        assert_eq!(f.mobile.as_deref(), Some("+49 176 1234567"));
        assert_eq!(f.phone.as_deref(), Some("040 123456"));
        assert_eq!(f.email.as_deref(), Some("erika@example.com"));
        assert_eq!(f.note.as_deref(), Some("Kunde seit 2020, wichtig"));
        assert!(parse_vcard("BEGIN:VCARD\r\nFN:a\r\nEND:VCARD\r\nBEGIN:VCARD\r\nFN:b\r\nEND:VCARD\r\n").is_none());
        assert!(parse_vcard("hello").is_none());
        // vCard 4 with tel: URI and only N
        let f = parse_vcard("BEGIN:VCARD\nVERSION:4.0\nN:Doe;John;;;\nTEL;VALUE=uri;TYPE=\"voice,cell\":tel:+491701234\nEND:VCARD\n").unwrap();
        assert_eq!(f.name, "John Doe");
        assert_eq!(f.mobile.as_deref(), Some("+491701234"));
    }

    #[test]
    fn patch_keeps_unknown_fields() {
        let old = parse_vcard(IOS).unwrap();
        let mut new = old.clone();
        new.mobile = Some("+49 171 7654321".into());
        new.note = None;
        let out = patch_vcard(IOS, &old, &new);
        let f = parse_vcard(&out).unwrap();
        assert_eq!(f.mobile.as_deref(), Some("+49 171 7654321"));
        assert_eq!(f.note, None);
        assert_eq!(f.phone, old.phone);
        assert!(out.contains("ADR;type=WORK:;;Hauptstr. 1"));
        assert!(out.contains("PHOTO;ENCODING=b;TYPE=JPEG:AAAA"));
        assert!(out.contains("TEL;type=WORK;type=FAX:040 999"));
        assert!(!out.contains("1234567"));
        // unchanged edit keeps the card's content
        assert_eq!(parse_vcard(&patch_vcard(IOS, &old, &old)).unwrap(), old);
    }

    #[test]
    fn preconditions_and_names() {
        let mut h = HeaderMap::new();
        assert!(precondition_ok(&h, None));
        h.insert("if-none-match", "*".parse().unwrap());
        assert!(precondition_ok(&h, None));
        assert!(!precondition_ok(&h, Some("\"a\"")));
        let mut h = HeaderMap::new();
        h.insert("if-match", "\"a\"".parse().unwrap());
        assert!(precondition_ok(&h, Some("\"a\"")));
        assert!(!precondition_ok(&h, Some("\"b\"")));
        assert!(!precondition_ok(&h, None));
        assert!(valid_res("0F3A-11.vcf"));
        // reserved for cards without a client name; an existing one keeps it on PUT (put_contact)
        assert!(!valid_res("c12.vcf"));
        assert!(valid_res("c12x.vcf"));
        assert!(!valid_res("u12.vcf"));
        assert!(!valid_res("../x"));
        assert!(!valid_res(".hidden"));
        assert!(!valid_res("svc-8000.vcf"));
    }

    #[test]
    fn service_cards_are_stable_and_grouped() {
        let emergency = vec!["112".to_string()];
        let de = crate::help::fixed_services(&emergency, Tr(Lang::De));
        let cards = service_cards(&de, "Dienste", "voip.example");
        let vm = cards.iter().find(|c| c.res == "svc-s97.vcf").expect("voicemail *97");
        assert!(vm.vcard.contains("UID:svc-s97\r\n"));
        assert!(vm.vcard.contains("FN:Mailbox (*97)\r\n"));
        assert!(vm.vcard.contains("ORG:Dienste\r\nCATEGORIES:Dienste\r\n"));
        // Linphone (belcard vCard 3.0 grammar) drops cards with any X- property
        assert!(!vm.vcard.contains("\r\nX-"));
        assert!(vm.vcard.contains("TEL;TYPE=WORK:*97\r\n"));
        assert!(vm.vcard.contains("IMPP:sip:*97@voip.example\r\n"));
        // the card parses like any other and the etag only depends on the content
        let f = parse_vcard(&vm.vcard).unwrap();
        assert_eq!(f.name, "Mailbox (*97)");
        assert_eq!(f.phone.as_deref(), Some("*97"));
        let again = service_cards(&crate::help::fixed_services(&emergency, Tr(Lang::De)), "Dienste", "voip.example");
        assert_eq!(ctag(&cards, ""), ctag(&again, ""));
        // force resync: another epoch changes the ctag, none keeps the old one
        assert_ne!(ctag(&cards, ""), ctag(&cards, "1"));
        let all: String = cards.iter().map(|c| format!("{}{}", c.res, c.etag())).collect();
        assert_eq!(ctag(&cards, ""), md5_hex(&all)[..16]);
        assert_eq!(vm.etag(), again.iter().find(|c| c.res == "svc-s97.vcf").unwrap().etag());
        // '#' is percent-encoded in the SIP URI, kept in TEL
        let fwd = cards.iter().find(|c| c.res == "svc-h21h.vcf").unwrap();
        assert!(fwd.vcard.contains("TEL;TYPE=WORK:#21#\r\n") && fwd.vcard.contains("IMPP:sip:%2321%23@voip.example\r\n"));
        assert!(cards.iter().any(|c| c.vcard.contains("FN:Notruf 112\r\n")));
        // searchable by name, digits and group name
        assert!(vm.matches(&["mailbox".into()]) && vm.matches(&["97".into()]) && vm.matches(&["dienste".into()]));
        // other language or content -> other etag and ctag
        let en = service_cards(&crate::help::fixed_services(&emergency, Tr(Lang::En)), "Services", "voip.example");
        assert!(en.iter().any(|c| c.vcard.contains("FN:Voicemail (*97)\r\n")));
        assert_ne!(ctag(&cards, ""), ctag(&en, ""));
        let more = service_cards(&crate::help::fixed_services(&["110".into(), "112".into()], Tr(Lang::De)), "Dienste", "voip.example");
        assert_ne!(ctag(&cards, ""), ctag(&more, ""));
        // escaping of names from the database
        let s = crate::help::Service {
            uid: "svc-8000".into(),
            number: "8000".into(),
            name: "Sales, North; 1".into(),
            kind: "Call group".into(),
            note: None,
        };
        let v = service_vcard(&s, "Services", "voip.example");
        assert!(v.contains("FN:Sales\\, North\\; 1\r\n"));
        assert_eq!(parse_vcard(&v).unwrap().name, "Sales, North; 1");
    }
}
