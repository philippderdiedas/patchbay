// SPDX-License-Identifier: GPL-3.0-or-later
//! External phone books: read-only CardDAV address books of other systems (an ERP or CRM, e.g.
//! Dolibarr through doli-carddav-bridge), loaded periodically into external_contacts.
//!
//! Configuration (environment):
//! * `PBX_EXTERNAL_BOOKS=erp,crm` – ids of the books (letters, digits, -)
//! * `PBX_EXTERNAL_BOOK_<ID>_URL` – URL of the address book collection (required)
//! * `PBX_EXTERNAL_BOOK_<ID>_NAME` – display name (default: the id)
//! * `PBX_EXTERNAL_BOOK_<ID>_USER`, `…_PASSWORD` or `…_PASSWORD_FILE` – HTTP Basic login
//! * `PBX_EXTERNAL_BOOK_<ID>_REFRESH_S` – seconds between loads (default 300, at least 60)
//!
//! Loading: PROPFIND Depth 0 (getctag; unchanged → nothing to do), PROPFIND Depth 1 (href + etag
//! of every card), REPORT addressbook-multiget for new or changed cards (100 per request). Works
//! with any CardDAV server. The local copy keeps caller names and the phone book working while the
//! other system is down. Shown read-only: phone book page, CardDAV (/carddav/ext-<id>/, also in
//! /carddav/book/), caller names after the person's own contacts and the shared phone book.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use quick_xml::{Reader, events::Event};

use crate::AppState;

/// Cards fetched per addressbook-multiget
const BATCH: usize = 100;
/// Cards per book at most (protects the database against a misconfigured URL)
const MAX_CARDS: usize = 50000;

#[derive(Clone, Debug)]
pub struct ExtBook {
    /// lower case id: CardDAV path /carddav/ext-<id>/, key in external_contacts
    pub id: String,
    pub name: String,
    pub url: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub refresh_s: u64,
}

pub fn from_env() -> Result<Vec<ExtBook>> {
    let list = std::env::var("PBX_EXTERNAL_BOOKS").unwrap_or_default();
    let mut out: Vec<ExtBook> = vec![];
    for raw in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let id = raw.to_lowercase();
        if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') || id.len() > 30 {
            bail!("PBX_EXTERNAL_BOOKS: {raw}: letters, digits and - only");
        }
        if out.iter().any(|b| b.id == id) {
            bail!("PBX_EXTERNAL_BOOKS: {raw} twice");
        }
        let key = |s: &str| format!("PBX_EXTERNAL_BOOK_{}_{s}", id.to_uppercase().replace('-', "_"));
        let get = |s: &str| std::env::var(key(s)).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let url = get("URL").with_context(|| format!("{} missing", key("URL")))?;
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            bail!("{}: http(s) URL expected", key("URL"));
        }
        let password = match (get("PASSWORD"), get("PASSWORD_FILE")) {
            (Some(p), _) => Some(p),
            (None, Some(f)) => Some(std::fs::read_to_string(&f).with_context(|| format!("{}: {f} not readable", key("PASSWORD_FILE")))?.trim().to_string()),
            _ => None,
        };
        out.push(ExtBook {
            name: get("NAME").unwrap_or_else(|| raw.to_string()),
            url: if url.ends_with('/') { url } else { format!("{url}/") },
            user: get("USER"),
            password,
            refresh_s: get("REFRESH_S").and_then(|v| v.parse().ok()).unwrap_or(300u64).max(60),
            id,
        });
    }
    Ok(out)
}

/// One `<response>` of a multistatus: href, etag, card text
#[derive(Debug, Default, PartialEq)]
pub struct DavItem {
    pub href: String,
    pub etag: Option<String>,
    pub data: Option<String>,
    /// resourcetype contains a collection (the book itself)
    pub collection: bool,
    pub ctag: Option<String>,
}

/// Parses a multistatus (namespace prefixes vary by server: compare local names only).
pub fn parse_multistatus(xml: &str) -> Result<Vec<DavItem>> {
    let mut r = Reader::from_str(xml);
    let mut out = vec![];
    let mut cur: Option<DavItem> = None;
    // local name of the open element whose text we want
    let mut field: Option<&'static str> = None;
    let mut text = String::new();
    loop {
        match r.read_event().context("CardDAV response is not valid XML")? {
            Event::Start(e) => {
                let name = e.local_name();
                match name.as_ref() {
                    b"response" => cur = Some(DavItem::default()),
                    b"collection" => {
                        if let Some(c) = cur.as_mut() {
                            c.collection = true;
                        }
                    }
                    b"href" | b"getetag" | b"address-data" | b"getctag" if cur.is_some() => {
                        field = Some(match name.as_ref() {
                            b"href" => "href",
                            b"getetag" => "getetag",
                            b"getctag" => "getctag",
                            _ => "address-data",
                        });
                        text.clear();
                    }
                    _ => {}
                }
            }
            Event::Empty(e) => {
                if e.local_name().as_ref() == b"collection" {
                    if let Some(c) = cur.as_mut() {
                        c.collection = true;
                    }
                }
            }
            Event::Text(t) => {
                if field.is_some() {
                    text.push_str(&t.decode().context("bad XML text")?);
                }
            }
            // entity and character references come as their own events (&amp;, &#13;)
            Event::GeneralRef(r) => {
                if field.is_some() {
                    if let Some(c) = r.resolve_char_ref().context("bad character reference")? {
                        text.push(c);
                    } else {
                        let name = r.decode().context("bad entity")?;
                        match quick_xml::escape::resolve_predefined_entity(&name) {
                            Some(v) => text.push_str(v),
                            None => bail!("unknown XML entity &{name};"),
                        }
                    }
                }
            }
            Event::CData(t) => {
                if field.is_some() {
                    text.push_str(&String::from_utf8_lossy(&t));
                }
            }
            Event::End(e) => {
                let name = e.local_name();
                if let (Some(f), Some(c)) = (field, cur.as_mut()) {
                    let matches = matches!((f, name.as_ref()), ("href", b"href") | ("getetag", b"getetag") | ("getctag", b"getctag") | ("address-data", b"address-data"));
                    if matches {
                        let v = text.trim().to_string();
                        match f {
                            // the first href of a response is its own (a <d:current-user-principal><d:href> comes later)
                            "href" if c.href.is_empty() => c.href = v,
                            "getetag" => c.etag = Some(v),
                            "getctag" => c.ctag = Some(v),
                            "address-data" => c.data = Some(v),
                            _ => {}
                        }
                        field = None;
                    }
                }
                if name.as_ref() == b"response" {
                    if let Some(c) = cur.take() {
                        out.push(c);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

/// The path part of an href (servers answer with absolute paths or full URLs)
fn href_path(h: &str) -> String {
    let p = match h.find("://") {
        Some(i) => h[i + 3..].find('/').map(|j| &h[i + 3 + j..]).unwrap_or("/"),
        None => h,
    };
    p.to_string()
}

/// The last path segment of an href (resource name, percent-decoded)
pub fn res_name(h: &str) -> String {
    let p = href_path(h);
    let seg = p.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string();
    percent_decode(&seg)
}

fn percent_decode(s: &str) -> String {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A card as stored: the fields Patchbay uses plus the link (vCard URL) and the card itself
#[derive(Debug, PartialEq)]
pub struct ExtCard {
    pub name: String,
    pub organization: Option<String>,
    pub phone: Option<String>,
    pub mobile: Option<String>,
    pub email: Option<String>,
    pub url: Option<String>,
}

pub fn card_fields(vcard: &str) -> Option<ExtCard> {
    let f = crate::carddav::parse_vcard(vcard)?;
    let url = crate::carddav::unfold(vcard).iter().find_map(|l| {
        let (name, _, value) = crate::carddav::split_line(l)?;
        let v = crate::carddav::unesc(value);
        (name == "URL" && (v.starts_with("https://") || v.starts_with("http://"))).then(|| v.chars().take(500).collect())
    });
    Some(ExtCard { name: f.name, organization: f.organization, phone: f.phone, mobile: f.mobile, email: f.email, url })
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(std::time::Duration::from_secs(60)).user_agent(concat!("patchbay/", env!("CARGO_PKG_VERSION"))).build()?)
}

async fn dav(http: &reqwest::Client, b: &ExtBook, method: &str, url: &str, depth: &str, body: String) -> Result<String> {
    let mut req = http
        .request(reqwest::Method::from_bytes(method.as_bytes())?, url)
        .header("Depth", depth)
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(body);
    if let Some(u) = &b.user {
        req = req.basic_auth(u, b.password.as_ref());
    }
    let r = req.send().await.with_context(|| format!("{method} {url}"))?;
    let status = r.status();
    if status.as_u16() != 207 {
        bail!("{method} {url}: HTTP {status}");
    }
    Ok(r.text().await?)
}

/// Loads one book; returns the number of cards, or None when the ctag was unchanged.
pub async fn sync(app: &AppState, b: &ExtBook) -> Result<Option<usize>> {
    let http = client()?;
    let props = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/"><d:prop><cs:getctag/><d:getetag/><d:resourcetype/></d:prop></d:propfind>"#;
    let head = parse_multistatus(&dav(&http, b, "PROPFIND", &b.url, "0", props.into()).await?)?;
    let ctag = head.iter().find_map(|i| i.ctag.clone());
    let stored: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT ctag, error FROM external_books WHERE book = $1").bind(&b.id).fetch_optional(&app.db).await?;
    if let (Some(c), Some((Some(old), None))) = (&ctag, &stored) {
        if c == old {
            sqlx::query("UPDATE external_books SET checked_at = now() WHERE book = $1").bind(&b.id).execute(&app.db).await?;
            return Ok(None);
        }
    }
    // href + etag of every card
    let list = parse_multistatus(&dav(&http, b, "PROPFIND", &b.url, "1", props.into()).await?)?;
    let base_path = href_path(&b.url);
    let remote: Vec<(String, String, String)> = list
        .into_iter()
        .filter(|i| !i.collection && href_path(&i.href).trim_end_matches('/') != base_path.trim_end_matches('/'))
        .map(|i| (res_name(&i.href), i.href, i.etag.unwrap_or_default()))
        .filter(|(res, _, _)| !res.is_empty())
        .collect();
    if remote.len() > MAX_CARDS {
        bail!("{} cards, at most {MAX_CARDS}", remote.len());
    }
    let local: HashMap<String, String> =
        sqlx::query_as::<_, (String, String)>("SELECT res, COALESCE(etag, '') FROM external_contacts WHERE book = $1").bind(&b.id).fetch_all(&app.db).await?.into_iter().collect();
    let changed: Vec<&(String, String, String)> = remote.iter().filter(|(res, _, etag)| etag.is_empty() || local.get(res) != Some(etag)).collect();
    // fetch new and changed cards
    let mut cards: Vec<(String, String, String)> = vec![]; // (res, etag, vcard)
    for chunk in changed.chunks(BATCH) {
        let hrefs: String = chunk.iter().map(|(_, h, _)| format!("<d:href>{}</d:href>", h.replace('&', "&amp;").replace('<', "&lt;"))).collect();
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><c:addressbook-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav"><d:prop><d:getetag/><c:address-data/></d:prop>{hrefs}</c:addressbook-multiget>"#
        );
        for i in parse_multistatus(&dav(&http, b, "REPORT", &b.url, "1", body).await?)? {
            if let Some(data) = i.data {
                cards.push((res_name(&i.href), i.etag.unwrap_or_default(), data));
            }
        }
    }
    let keep: Vec<String> = remote.iter().map(|(r, _, _)| r.clone()).collect();
    let mut tx = app.db.begin().await?;
    sqlx::query("DELETE FROM external_contacts WHERE book = $1 AND NOT (res = ANY($2))").bind(&b.id).bind(&keep).execute(&mut *tx).await?;
    for (res, etag, vcard) in &cards {
        let Some(f) = card_fields(vcard) else { continue };
        let (phone, phone_e164) = crate::carddav::store_number(&f.phone);
        let (mobile, mobile_e164) = crate::carddav::store_number(&f.mobile);
        sqlx::query(
            "INSERT INTO external_contacts (book, res, etag, name, organization, phone, phone_e164, mobile, mobile_e164, email, url, vcard)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             ON CONFLICT (book, res) DO UPDATE SET etag = EXCLUDED.etag, name = EXCLUDED.name, organization = EXCLUDED.organization,
               phone = EXCLUDED.phone, phone_e164 = EXCLUDED.phone_e164, mobile = EXCLUDED.mobile, mobile_e164 = EXCLUDED.mobile_e164,
               email = EXCLUDED.email, url = EXCLUDED.url, vcard = EXCLUDED.vcard",
        )
        .bind(&b.id)
        .bind(res)
        .bind(etag)
        .bind(&f.name)
        .bind(&f.organization)
        .bind(&phone)
        .bind(&phone_e164)
        .bind(&mobile)
        .bind(&mobile_e164)
        .bind(&f.email)
        .bind(&f.url)
        .bind(vcard)
        .execute(&mut *tx)
        .await?;
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM external_contacts WHERE book = $1").bind(&b.id).fetch_one(&mut *tx).await?;
    sqlx::query(
        "INSERT INTO external_books (book, ctag, synced_at, checked_at, error, cards) VALUES ($1, $2, now(), now(), NULL, $3)
         ON CONFLICT (book) DO UPDATE SET ctag = EXCLUDED.ctag, synced_at = now(), checked_at = now(), error = NULL, cards = EXCLUDED.cards",
    )
    .bind(&b.id)
    .bind(&ctag)
    .bind(n as i32)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(n as usize))
}

/// Removes copies of books no longer configured and starts one loader per book.
pub async fn spawn(app: AppState) -> Result<()> {
    let ids: Vec<String> = app.cfg.external_books.iter().map(|b| b.id.clone()).collect();
    sqlx::query("DELETE FROM external_contacts WHERE NOT (book = ANY($1))").bind(&ids).execute(&app.db).await?;
    sqlx::query("DELETE FROM external_books WHERE NOT (book = ANY($1))").bind(&ids).execute(&app.db).await?;
    for b in app.cfg.external_books.clone() {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                match sync(&app, &b).await {
                    Ok(Some(n)) => tracing::info!("external phone book {}: {n} entries loaded", b.id),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!("external phone book {}: {e:#}", b.id);
                        let msg: String = format!("{e:#}").chars().take(300).collect();
                        let _ = sqlx::query(
                            "INSERT INTO external_books (book, checked_at, error) VALUES ($1, now(), $2)
                             ON CONFLICT (book) DO UPDATE SET checked_at = now(), error = EXCLUDED.error",
                        )
                        .bind(&b.id)
                        .bind(&msg)
                        .execute(&app.db)
                        .await;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(b.refresh_s)).await;
            }
        });
    }
    Ok(())
}

// --- Reading ---------------------------------------------------------------------------------------

#[derive(sqlx::FromRow, Clone, Debug)]
pub struct ExtContact {
    pub res: String,
    pub name: String,
    pub organization: Option<String>,
    pub phone: Option<String>,
    pub mobile: Option<String>,
    pub email: Option<String>,
    pub url: Option<String>,
}

impl ExtContact {
    pub fn org(&self) -> String {
        self.organization.clone().unwrap_or_default()
    }
}

/// A configured book with its entries and load state (phone book page)
pub struct BookView {
    pub id: String,
    pub name: String,
    pub entries: Vec<ExtContact>,
    /// "23.11.26 14:03", empty: never loaded
    pub synced: String,
    pub error: Option<String>,
}

pub async fn books(app: &AppState) -> anyhow::Result<Vec<BookView>> {
    let mut out = vec![];
    for b in &app.cfg.external_books {
        let entries: Vec<ExtContact> = sqlx::query_as(
            "SELECT res, name, organization, phone, mobile, email, url FROM external_contacts WHERE book = $1 ORDER BY lower(name), res",
        )
        .bind(&b.id)
        .fetch_all(&app.db)
        .await?;
        let state: Option<(Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT to_char(synced_at AT TIME ZONE $2, 'DD.MM.YY HH24:MI'), error FROM external_books WHERE book = $1",
        )
        .bind(&b.id)
        .bind(&app.cfg.timezone)
        .fetch_optional(&app.db)
        .await?;
        let (synced, error) = state.unwrap_or((None, None));
        out.push(BookView { id: b.id.clone(), name: b.name.clone(), entries, synced: synced.unwrap_or_default(), error });
    }
    Ok(out)
}

/// Stored cards of a book for the CardDAV server: (res, vcard)
pub async fn cards(app: &AppState, book: &str) -> anyhow::Result<Vec<(String, String)>> {
    Ok(sqlx::query_as("SELECT res, vcard FROM external_contacts WHERE book = $1 ORDER BY lower(name), res").bind(book).fetch_all(&app.db).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multistatus_with_any_prefix() {
        let xml = r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav" xmlns:x="http://calendarserver.org/ns/">
          <D:response><D:href>/carddav/dolibarr/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/><C:addressbook/></D:resourcetype>
            <D:current-user-principal><D:href>/carddav/</D:href></D:current-user-principal><x:getctag>abc</x:getctag></D:prop></D:propstat></D:response>
          <D:response><D:href>https://h.example/carddav/dolibarr/soc-1.vcf</D:href><D:propstat><D:prop><D:getetag>"e1"</D:getetag>
            <C:address-data>BEGIN:VCARD&#13;
VERSION:3.0&#13;
FN:A &amp; B&#13;
END:VCARD</C:address-data></D:prop></D:propstat></D:response>
          <response xmlns="DAV:"><href>/carddav/dolibarr/c%20d.vcf</href><propstat><prop><getetag>"e2"</getetag><address-data xmlns="urn:ietf:params:xml:ns:carddav"><![CDATA[BEGIN:VCARD
FN:<x>
END:VCARD]]></address-data></prop></propstat></response>
        </D:multistatus>"#;
        let items = parse_multistatus(xml).unwrap();
        assert_eq!(items.len(), 3);
        assert!(items[0].collection);
        assert_eq!(items[0].href, "/carddav/dolibarr/", "own href, not the principal's");
        assert_eq!(items[0].ctag.as_deref(), Some("abc"));
        assert_eq!(items[1].etag.as_deref(), Some("\"e1\""));
        assert!(items[1].data.as_deref().unwrap().contains("FN:A & B"));
        assert_eq!(res_name(&items[1].href), "soc-1.vcf");
        assert_eq!(res_name(&items[2].href), "c d.vcf");
        assert!(items[2].data.as_deref().unwrap().contains("FN:<x>"));
    }

    #[test]
    fn fields_and_link() {
        let v = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:ACME GmbH\r\nORG:ACME GmbH\r\nTEL;TYPE=WORK,VOICE:040 123456\r\nTEL;TYPE=CELL:0176 1\r\nURL:https://erp.example/societe/card.php?socid=1\r\nEND:VCARD\r\n";
        let f = card_fields(v).unwrap();
        assert_eq!(f.name, "ACME GmbH");
        assert_eq!(f.phone.as_deref(), Some("040 123456"));
        assert_eq!(f.mobile.as_deref(), Some("0176 1"));
        assert_eq!(f.url.as_deref(), Some("https://erp.example/societe/card.php?socid=1"));
        // no javascript: or other schemes as links
        assert_eq!(card_fields(&v.replace("https://erp.example", "javascript:alert(1)//")).unwrap().url, None);
    }

    /// Real answers of a CardDAV server (head.xml, list.xml, multi.xml in $EXTBOOK_SAMPLES), e.g.
    /// captured from doli-carddav-bridge: `cargo test extbooks -- --ignored`
    #[test]
    #[ignore]
    fn real_server_samples() {
        let dir = std::env::var("EXTBOOK_SAMPLES").expect("EXTBOOK_SAMPLES");
        let read = |f: &str| std::fs::read_to_string(format!("{dir}/{f}")).unwrap();
        let head = parse_multistatus(&read("head.xml")).unwrap();
        assert!(head[0].collection && head[0].ctag.is_some(), "book with ctag");
        let list = parse_multistatus(&read("list.xml")).unwrap();
        let cards: Vec<&DavItem> = list.iter().filter(|i| !i.collection).collect();
        assert!(cards.len() > 10 && cards.iter().all(|i| i.etag.is_some() && res_name(&i.href).ends_with(".vcf")));
        let multi = parse_multistatus(&read("multi.xml")).unwrap();
        let parsed: Vec<ExtCard> = multi.iter().filter_map(|i| i.data.as_deref().and_then(card_fields)).collect();
        assert_eq!(parsed.len(), multi.len(), "every card parses");
        let with_phone = parsed.iter().filter(|c| c.phone.is_some() || c.mobile.is_some()).count();
        let with_url = parsed.iter().filter(|c| c.url.is_some()).count();
        eprintln!("{} cards in list, {} fetched, {with_phone} with a number, {with_url} with a link", cards.len(), parsed.len());
        assert!(with_phone > 0 && with_url == parsed.len());
    }

    #[test]
    fn href_paths() {
        assert_eq!(href_path("https://x.example/a/b/"), "/a/b/");
        assert_eq!(href_path("/a/b/c.vcf"), "/a/b/c.vcf");
        assert_eq!(res_name("/a/b/"), "b");
    }
}
