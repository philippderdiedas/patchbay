// SPDX-License-Identifier: GPL-3.0-or-later
//! Public holiday calendar of the site (table `holidays`, calendar "site"), used by the time
//! profiles that have public holidays switched on. Empty unless an admin enters holidays by hand or
//! imports them from a source:
//! - Nager.Date (`PBX_HOLIDAY_NAGER_URL`, default https://date.nager.at): country + optional subdivision,
//! - iCal: URL or uploaded .ics file.
//!
//! An import covers the current and the next year. Sources marked "keep updated" (URL sources only)
//! are re-imported daily; a re-import only replaces rows of that source, never manual ones, and keeps
//! rows the admin deleted hidden. On a fetch failure the existing rows stay. Routing only reads the
//! table (`time_profile_state`).

use std::{
    collections::BTreeMap,
    sync::{LazyLock, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use askama::Template;
use axum::{
    Form,
    extract::{Multipart, Path, Query, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::{Datelike, NaiveDate};
use serde::Deserialize;
use sqlx::{FromRow, PgPool};

use crate::{AppError, AppState, auth::Admin, db, ui::Ctx};

/// Calendar key in `holidays.region` and `time_profiles.holiday_region` (profiles: 'site' or 'none').
pub const CALENDAR: &str = "site";
/// Largest .ics accepted (URL or upload)
const MAX_ICS: usize = 2 * 1024 * 1024;
/// Longest all-day event expanded into single days
const MAX_EVENT_DAYS: i64 = 31;

pub type Entries = Vec<(NaiveDate, String)>;

fn years() -> [i32; 2] {
    let y = chrono::Local::now().year();
    [y, y + 1]
}

// --- iCal ------------------------------------------------------------------------------------

/// Result of parsing an .ics file: days with names (sorted, one per date) and skipped events.
#[derive(Debug, Default)]
pub struct IcsResult {
    pub entries: Entries,
    pub skipped: usize,
}

/// RFC 5545 line unfolding: a line starting with a space or tab continues the previous one.
fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = vec![];
    for raw in text.split('\n') {
        let l = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = l.strip_prefix(' ').or_else(|| l.strip_prefix('\t')) {
            if let Some(last) = lines.last_mut() {
                last.push_str(rest);
                continue;
            }
        }
        lines.push(l.to_string());
    }
    lines
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') | Some('N') => out.push(' '),
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

/// DTSTART/DTEND value: `20261225` (VALUE=DATE) or `20261225T000000[Z]`. Returns the date and
/// whether it was a plain date.
fn ics_date(v: &str) -> Option<(NaiveDate, bool)> {
    let d = NaiveDate::parse_from_str(v.get(..8)?, "%Y%m%d").ok()?;
    Some((d, v.len() == 8))
}

#[derive(Default)]
struct Event {
    start: Option<(NaiveDate, bool)>,
    end: Option<(NaiveDate, bool)>,
    summary: String,
    rrule: Option<String>,
    cancelled: bool,
}

/// Yearly recurrence the parser understands: `FREQ=YEARLY` with optional `INTERVAL=1`, `UNTIL`,
/// `COUNT` and BYMONTH/BYMONTHDAY equal to the start date. Returns the last year (None = open).
/// Err(()) for anything else.
fn yearly(rrule: &str, start: NaiveDate) -> std::result::Result<Option<i32>, ()> {
    let mut freq = false;
    let mut last = None;
    for part in rrule.split(';') {
        let (k, v) = part.split_once('=').ok_or(())?;
        match k.to_uppercase().as_str() {
            "FREQ" if v.eq_ignore_ascii_case("YEARLY") => freq = true,
            "INTERVAL" if v == "1" => {}
            "UNTIL" => last = Some(ics_date(v).ok_or(())?.0.year()),
            "COUNT" => last = Some(start.year() + v.parse::<i32>().map_err(|_| ())? - 1),
            "BYMONTH" if v.parse::<u32>().ok() == Some(start.month()) => {}
            "BYMONTHDAY" if v.parse::<u32>().ok() == Some(start.day()) => {}
            "WKST" => {}
            _ => return Err(()),
        }
    }
    if freq { Ok(last) } else { Err(()) }
}

/// Parses VEVENTs (DTSTART, DTEND, SUMMARY, simple yearly RRULE) into days of the given years.
/// Events without a usable start or with another recurrence are skipped and counted.
pub fn parse_ics(text: &str, years: &[i32]) -> IcsResult {
    let mut res = IcsResult::default();
    let mut days: BTreeMap<NaiveDate, String> = BTreeMap::new();
    let mut ev: Option<Event> = None;
    for line in unfold(text) {
        let Some((head, value)) = line.split_once(':') else { continue };
        let name = head.split(';').next().unwrap_or("").to_uppercase();
        match (name.as_str(), value.trim()) {
            ("BEGIN", v) if v.eq_ignore_ascii_case("VEVENT") => ev = Some(Event::default()),
            ("END", v) if v.eq_ignore_ascii_case("VEVENT") => {
                let Some(e) = ev.take() else { continue };
                if e.cancelled {
                    continue;
                }
                let Some((start, is_date)) = e.start else {
                    res.skipped += 1;
                    continue;
                };
                // all-day events: DTEND is exclusive; date-time events count for their start day
                let len = match e.end {
                    Some((end, true)) if is_date && end > start => (end - start).num_days().min(MAX_EVENT_DAYS),
                    _ => 1,
                };
                let name = if e.summary.is_empty() { "Holiday".to_string() } else { e.summary };
                let mut add = |first: NaiveDate| {
                    for i in 0..len {
                        let d = first + chrono::Days::new(i as u64);
                        if years.contains(&d.year()) {
                            days.entry(d).or_insert_with(|| name.clone());
                        }
                    }
                };
                match e.rrule.as_deref() {
                    None => add(start),
                    Some(r) => match yearly(r, start) {
                        Ok(last) => {
                            for &y in years {
                                if y >= start.year() && last.is_none_or(|l| y <= l) {
                                    // 29 February only in leap years
                                    if let Some(d) = NaiveDate::from_ymd_opt(y, start.month(), start.day()) {
                                        add(d);
                                    }
                                }
                            }
                        }
                        Err(()) => res.skipped += 1,
                    },
                }
            }
            (_, v) => {
                let Some(e) = ev.as_mut() else { continue };
                match name.as_str() {
                    "DTSTART" => e.start = ics_date(v),
                    "DTEND" => e.end = ics_date(v),
                    "SUMMARY" => e.summary = unescape(v),
                    "RRULE" => e.rrule = Some(v.to_string()),
                    "STATUS" => e.cancelled = v.eq_ignore_ascii_case("CANCELLED"),
                    _ => {}
                }
            }
        }
    }
    res.entries = days.into_iter().collect();
    res
}

// --- Nager.Date -------------------------------------------------------------------------------

#[derive(Deserialize)]
struct NagerHoliday {
    date: String,
    #[serde(rename = "localName")]
    local_name: Option<String>,
    name: Option<String>,
    counties: Option<Vec<String>>,
    types: Option<Vec<String>>,
}

/// Maps a Nager.Date `PublicHolidays` answer: public holidays only; without a subdivision only the
/// nationwide ones (`counties` null), with one (e.g. `DE-SH`) also those listing it.
pub fn map_nager(json: &str, subdivision: Option<&str>) -> Result<Entries> {
    let list: Vec<NagerHoliday> = serde_json::from_str(json).context("unexpected Nager.Date answer")?;
    let mut out = BTreeMap::new();
    for h in list {
        if let Some(t) = &h.types {
            if !t.iter().any(|t| t == "Public") {
                continue;
            }
        }
        let applies = match (&h.counties, subdivision) {
            (None, _) => true,
            (Some(c), Some(sub)) => c.iter().any(|c| c.eq_ignore_ascii_case(sub)),
            (Some(_), None) => false,
        };
        if !applies {
            continue;
        }
        let Ok(d) = NaiveDate::parse_from_str(&h.date, "%Y-%m-%d") else { continue };
        let name = h.local_name.or(h.name).unwrap_or_else(|| "Holiday".into());
        out.entry(d).or_insert(name);
    }
    Ok(out.into_iter().collect())
}

#[derive(Deserialize, Clone)]
pub struct Country {
    #[serde(rename = "countryCode")]
    pub code: String,
    pub name: String,
}

/// Countries of Nager.Date for the select (cached after the first successful fetch).
async fn countries(base: &str) -> Vec<Country> {
    static CACHE: LazyLock<Mutex<Vec<Country>>> = LazyLock::new(|| Mutex::new(vec![]));
    if let Ok(c) = CACHE.lock() {
        if !c.is_empty() {
            return c.clone();
        }
    }
    let fetched = async {
        let body = http()?.get(format!("{base}/api/v3/AvailableCountries")).send().await?.error_for_status()?.text().await?;
        let mut list: Vec<Country> = serde_json::from_str(&body)?;
        list.sort_by(|a, b| a.name.cmp(&b.name));
        anyhow::Ok(list)
    }
    .await;
    match fetched {
        Ok(list) => {
            if let Ok(mut c) = CACHE.lock() {
                *c = list.clone();
            }
            list
        }
        Err(e) => {
            tracing::warn!("Nager.Date country list: {e:#}");
            vec![]
        }
    }
}

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?)
}

// --- Sources and import -------------------------------------------------------------------------

#[derive(FromRow, Clone)]
pub struct Source {
    pub id: i32,
    pub kind: String,
    pub country: String,
    pub subdivision: String,
    pub url: Option<String>,
    pub keep_updated: bool,
    pub last_run: Option<String>,
    pub last_result: String,
    pub holidays: i64,
}

impl Source {
    pub fn label(&self) -> String {
        match self.kind.as_str() {
            "nager" if self.subdivision.is_empty() => format!("Nager.Date {}", self.country),
            "nager" => format!("Nager.Date {} {}", self.country, self.subdivision),
            _ => format!("iCal {}", self.url.as_deref().unwrap_or("(upload)")),
        }
    }
}

const SOURCE_SELECT: &str = "SELECT s.id, s.kind, s.country, s.subdivision, s.url, s.keep_updated,
        to_char(s.last_run, 'DD.MM.YYYY HH24:MI') AS last_run, s.last_result,
        (SELECT count(*) FROM holidays h WHERE h.source_id = s.id AND NOT h.hidden) AS holidays
    FROM holiday_sources s";

/// What to import (form or stored source).
pub struct Spec {
    pub kind: String,
    pub country: String,
    pub subdivision: String,
    pub url: String,
    pub file: Option<String>,
}

fn validate(s: &Spec) -> Result<(), AppError> {
    let bad = |m: &str| Err(AppError::BadRequest(m.into()));
    match s.kind.as_str() {
        "nager" => {
            if s.country.len() != 2 || !s.country.bytes().all(|b| b.is_ascii_uppercase()) {
                return bad("Country: two-letter code, e.g. DE.");
            }
            if !s.subdivision.is_empty()
                && !(s.subdivision.starts_with(&format!("{}-", s.country))
                    && s.subdivision.len() <= 10
                    && s.subdivision.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
            {
                return bad("Subdivision: ISO 3166-2 code of the country, e.g. DE-SH.");
            }
        }
        "ical" if s.file.is_some() => {}
        "ical" if s.url.starts_with("https://") || s.url.starts_with("http://") => {}
        "ical" => return bad("iCal: URL (http/https) or file missing."),
        _ => return bad("Unknown source."),
    }
    Ok(())
}

/// Fetches the holidays of a source for the current and the next year.
pub async fn fetch(nager_url: &str, s: &Spec) -> Result<IcsResult> {
    let ys = years();
    if s.kind == "nager" {
        let mut entries = vec![];
        for y in ys {
            let url = format!("{nager_url}/api/v3/PublicHolidays/{y}/{}", s.country);
            let body = http()?.get(&url).send().await?.error_for_status().with_context(|| url.clone())?.text().await?;
            entries.extend(map_nager(&body, (!s.subdivision.is_empty()).then_some(s.subdivision.as_str()))?);
        }
        return Ok(IcsResult { entries, skipped: 0 });
    }
    let text = match &s.file {
        Some(t) => t.clone(),
        None => {
            let resp = http()?.get(&s.url).send().await?.error_for_status()?;
            let bytes = resp.bytes().await?;
            anyhow::ensure!(bytes.len() <= MAX_ICS, "calendar larger than {} MB", MAX_ICS / 1024 / 1024);
            String::from_utf8_lossy(&bytes).into_owned()
        }
    };
    anyhow::ensure!(text.contains("BEGIN:VCALENDAR"), "not an iCalendar file");
    Ok(parse_ics(&text, &ys))
}

/// Stores the entries of a source for the current and the next year: rows of this source that are
/// no longer delivered go, new ones come, names are updated. Manual rows (and rows of other sources)
/// on the same date win; rows the admin deleted stay hidden. Returns the number of entries stored.
pub async fn store(db: &PgPool, source_id: i32, entries: &Entries) -> Result<u64> {
    let ys = years();
    let dates: Vec<NaiveDate> = entries.iter().map(|(d, _)| *d).collect();
    let mut tx = db.begin().await?;
    sqlx::query(
        "DELETE FROM holidays WHERE source_id = $1 AND extract(year FROM date)::int = ANY ($2) AND date <> ALL ($3)",
    )
    .bind(source_id)
    .bind(&ys[..])
    .bind(&dates)
    .execute(&mut *tx)
    .await?;
    let mut n = 0;
    for (date, name) in entries {
        let name: String = name.chars().take(80).collect();
        n += sqlx::query(
            "INSERT INTO holidays (region, date, name, source_id) VALUES ($1, $2, $3, $4)
             ON CONFLICT (region, date) DO UPDATE SET name = EXCLUDED.name WHERE holidays.source_id = EXCLUDED.source_id",
        )
        .bind(CALENDAR)
        .bind(date)
        .bind(&name)
        .bind(source_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    tx.commit().await?;
    Ok(n)
}

async fn mark(db: &PgPool, id: i32, result: &str) {
    let _ = sqlx::query("UPDATE holiday_sources SET last_run = now(), last_result = $2 WHERE id = $1")
        .bind(id)
        .bind(result)
        .execute(db)
        .await;
}

/// Re-imports one stored source; failures are recorded and logged, the rows stay.
async fn refresh(db: &PgPool, nager_url: &str, s: &Source) {
    let spec = Spec { kind: s.kind.clone(), country: s.country.clone(), subdivision: s.subdivision.clone(), url: s.url.clone().unwrap_or_default(), file: None };
    match fetch(nager_url, &spec).await {
        Ok(r) => match store(db, s.id, &r.entries).await {
            Ok(n) => mark(db, s.id, &format!("ok: {n} stored, {} skipped", r.skipped)).await,
            Err(e) => {
                tracing::warn!("holiday source {}: {e:#}", s.id);
                mark(db, s.id, &format!("error: {e:#}")).await;
            }
        },
        Err(e) => {
            tracing::warn!("holiday source {} ({}): {e:#}; existing holidays kept", s.id, s.label());
            mark(db, s.id, &format!("error: {e:#}")).await;
        }
    }
}

/// Daily re-import of the sources marked "keep updated".
pub fn spawn(db: PgPool, nager_url: String) {
    tokio::spawn(async move {
        loop {
            match sqlx::query_as::<_, Source>(&format!("{SOURCE_SELECT} WHERE s.keep_updated AND (s.url IS NOT NULL OR s.kind = 'nager') ORDER BY s.id"))
                .fetch_all(&db)
                .await
            {
                Ok(list) => {
                    for s in &list {
                        refresh(&db, &nager_url, s).await;
                    }
                }
                Err(e) => tracing::warn!("holiday sources: {e:#}"),
            }
            tokio::time::sleep(Duration::from_secs(86_400)).await;
        }
    });
}

// --- Admin page ---------------------------------------------------------------------------------

#[derive(FromRow)]
pub struct HolidayRow {
    date: NaiveDate,
    name: String,
    source_id: Option<i32>,
}

impl HolidayRow {
    fn date_str(&self) -> String {
        self.date.format("%d.%m.%Y").to_string()
    }
    fn iso(&self) -> String {
        self.date.to_string()
    }
}

pub struct Preview {
    label: String,
    entries: Vec<(String, String)>,
    skipped: usize,
}

#[derive(Template)]
#[template(path = "admin_holidays.html")]
struct HolidaysPage {
    c: Ctx,
    sources: Vec<Source>,
    holidays: Vec<HolidayRow>,
    hidden: i64,
    countries: Vec<Country>,
    nager_url: String,
    msg: String,
    preview: Option<Preview>,
}

impl HolidaysPage {
    fn source_label(&self, id: &Option<i32>) -> String {
        match id {
            None => String::new(),
            Some(id) => self.sources.iter().find(|s| s.id == *id).map(|s| s.label()).unwrap_or_default(),
        }
    }
}

#[derive(Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    msg: String,
}

async fn page(app: &AppState, a: &crate::auth::CurrentUser, msg: String, preview: Option<Preview>) -> Result<Response, AppError> {
    let sources = sqlx::query_as(&format!("{SOURCE_SELECT} ORDER BY s.id")).fetch_all(&app.db).await.map_err(anyhow::Error::from)?;
    let [y, _] = years();
    let holidays = sqlx::query_as(
        "SELECT date, name, source_id FROM holidays WHERE region = $1 AND NOT hidden
         AND date >= make_date($2, 1, 1) AND date < make_date($2 + 2, 1, 1) ORDER BY date",
    )
    .bind(CALENDAR)
    .bind(y)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let hidden: i64 = sqlx::query_scalar("SELECT count(*) FROM holidays WHERE region = $1 AND hidden AND NOT moved")
        .bind(CALENDAR)
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let nager_url = app.cfg.holiday_nager_url.clone();
    let html = HolidaysPage {
        c: Ctx::new(app, a).section("times"),
        sources,
        holidays,
        hidden,
        countries: countries(&nager_url).await,
        nager_url,
        msg,
        preview,
    }
    .render()
    .map_err(anyhow::Error::from)?;
    Ok(Html(html).into_response())
}

pub async fn holidays(State(app): State<AppState>, Admin(a): Admin, Query(q): Query<PageQuery>) -> Result<Response, AppError> {
    let msg: String = q.msg.chars().take(200).collect();
    page(&app, &a, msg, None).await
}

fn back(msg: &str) -> Response {
    let enc: String = msg.bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
    Redirect::to(&format!("/admin/holidays?msg={enc}")).into_response()
}

/// Import form (multipart, because of the .ics upload): action preview or import.
pub async fn import(State(app): State<AppState>, Admin(a): Admin, mut mp: Multipart) -> Result<Response, AppError> {
    let mut spec = Spec { kind: String::new(), country: String::new(), subdivision: String::new(), url: String::new(), file: None };
    let (mut keep, mut action) = (false, String::new());
    while let Some(field) = mp.next_field().await.map_err(|e| AppError::BadRequest(e.to_string()))? {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            let bytes = field.bytes().await.map_err(|e| AppError::BadRequest(e.to_string()))?;
            if !bytes.is_empty() {
                if bytes.len() > MAX_ICS {
                    return Err(AppError::BadRequest("Calendar file too large.".into()));
                }
                spec.file = Some(String::from_utf8_lossy(&bytes).into_owned());
            }
            continue;
        }
        let v = field.text().await.unwrap_or_default().trim().to_string();
        match name.as_str() {
            "kind" => spec.kind = v,
            "country" => spec.country = v.to_uppercase(),
            "subdivision" => spec.subdivision = v.to_uppercase(),
            "url" => spec.url = v,
            "keep" => keep = true,
            "action" => action = v,
            _ => {}
        }
    }
    validate(&spec)?;
    // an uploaded file cannot be re-imported
    if spec.file.is_some() {
        spec.url.clear();
        keep = false;
    }
    let label = Source {
        id: 0,
        kind: spec.kind.clone(),
        country: spec.country.clone(),
        subdivision: spec.subdivision.clone(),
        url: (!spec.url.is_empty()).then(|| spec.url.clone()),
        keep_updated: keep,
        last_run: None,
        last_result: String::new(),
        holidays: 0,
    }
    .label();
    let r = fetch(&app.cfg.holiday_nager_url, &spec).await.map_err(|e| AppError::BadRequest(format!("{label}: {e:#}")))?;
    if action == "preview" {
        let entries = r.entries.iter().map(|(d, n)| (d.format("%d.%m.%Y").to_string(), n.clone())).collect();
        return page(&app, &a, String::new(), Some(Preview { label, entries, skipped: r.skipped })).await;
    }
    // same source again: reuse it (and take the new keep-updated choice)
    let existing: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM holiday_sources WHERE kind = $1 AND country = $2 AND subdivision = $3 AND url IS NOT DISTINCT FROM $4",
    )
    .bind(&spec.kind)
    .bind(&spec.country)
    .bind(&spec.subdivision)
    .bind((!spec.url.is_empty()).then_some(&spec.url))
    .fetch_optional(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let id = match existing.filter(|_| spec.file.is_none()) {
        Some(id) => {
            sqlx::query("UPDATE holiday_sources SET keep_updated = $2 WHERE id = $1").bind(id).bind(keep).execute(&app.db).await.map_err(anyhow::Error::from)?;
            id
        }
        None => sqlx::query_scalar(
            "INSERT INTO holiday_sources (kind, country, subdivision, url, keep_updated) VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(&spec.kind)
        .bind(&spec.country)
        .bind(&spec.subdivision)
        .bind((!spec.url.is_empty()).then_some(&spec.url))
        .bind(keep)
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?,
    };
    let n = store(&app.db, id, &r.entries).await?;
    let result = format!("ok: {n} stored, {} skipped", r.skipped);
    mark(&app.db, id, &result).await;
    db::audit(&app.db, &a.session.sub, "holiday.import", &format!("holiday-source:{id}"), serde_json::json!({"source": label, "stored": n, "skipped": r.skipped})).await?;
    Ok(back(&format!("{label}: {n} holidays stored, {} events skipped.", r.skipped)))
}

#[derive(Deserialize)]
pub struct KeepForm {
    #[serde(default)]
    keep: Option<String>,
}

pub async fn source_keep(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>, Form(f): Form<KeepForm>) -> Result<Response, AppError> {
    let keep = f.keep.is_some();
    sqlx::query("UPDATE holiday_sources SET keep_updated = $2 AND (url IS NOT NULL OR kind = 'nager') WHERE id = $1")
        .bind(id)
        .bind(keep)
        .execute(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "holiday.source", &format!("holiday-source:{id}"), serde_json::json!({"keep_updated": keep})).await?;
    Ok(Redirect::to("/admin/holidays").into_response())
}

pub async fn source_refresh(State(app): State<AppState>, Admin(_a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let s: Source = sqlx::query_as(&format!("{SOURCE_SELECT} WHERE s.id = $1"))
        .bind(id)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or(AppError::NotFound)?;
    if s.kind == "ical" && s.url.is_none() {
        return Err(AppError::BadRequest("An uploaded file cannot be refreshed; upload it again.".into()));
    }
    refresh(&app.db, &app.cfg.holiday_nager_url, &s).await;
    let r: String = sqlx::query_scalar("SELECT last_result FROM holiday_sources WHERE id = $1").bind(id).fetch_one(&app.db).await.map_err(anyhow::Error::from)?;
    Ok(back(&format!("{}: {r}", s.label())))
}

/// Removes a source together with the holidays it imported (manual entries stay).
pub async fn source_delete(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    sqlx::query("DELETE FROM holiday_sources WHERE id = $1").bind(id).execute(&app.db).await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "holiday.source.delete", &format!("holiday-source:{id}"), serde_json::json!({})).await?;
    Ok(Redirect::to("/admin/holidays").into_response())
}

#[derive(Deserialize)]
pub struct HolidayForm {
    date: NaiveDate,
    name: String,
    /// set when editing (Edit button): the holiday on that date is moved to `date`
    #[serde(default)]
    old_date: String,
}

/// Adds a manual holiday or edits one, also its date (an edited imported holiday becomes manual;
/// when its date changes, the imported entry stays hidden, so the next import does not bring it back).
pub async fn holiday_save(State(app): State<AppState>, Admin(a): Admin, Form(f): Form<HolidayForm>) -> Result<Response, AppError> {
    let name: String = f.name.trim().chars().take(80).collect();
    if name.is_empty() {
        return Err(AppError::BadRequest("Name missing.".into()));
    }
    let old: Option<NaiveDate> = match f.old_date.trim() {
        "" => None,
        d => Some(NaiveDate::parse_from_str(d, "%Y-%m-%d").map_err(|_| AppError::BadRequest("Invalid date.".into()))?),
    };
    let mut tx = app.db.begin().await.map_err(anyhow::Error::from)?;
    if let Some(old) = old.filter(|o| *o != f.date) {
        // another (visible) holiday on the new date: refuse instead of overwriting it
        let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM holidays WHERE region = $1 AND date = $2 AND NOT hidden)")
            .bind(CALENDAR)
            .bind(f.date)
            .fetch_one(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
        if taken {
            return Err(AppError::BadRequest(format!("There is already a holiday on {}.", f.date.format("%d.%m.%Y"))));
        }
        let gone = sqlx::query("DELETE FROM holidays WHERE region = $1 AND date = $2 AND source_id IS NULL AND NOT hidden")
            .bind(CALENDAR)
            .bind(old)
            .execute(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?
            .rows_affected()
            + sqlx::query("UPDATE holidays SET hidden = true, moved = true WHERE region = $1 AND date = $2 AND NOT hidden")
                .bind(CALENDAR)
                .bind(old)
                .execute(&mut *tx)
                .await
                .map_err(anyhow::Error::from)?
                .rows_affected();
        if gone == 0 {
            return Err(AppError::BadRequest("The holiday no longer exists. Please reload the page.".into()));
        }
    }
    sqlx::query(
        "INSERT INTO holidays (region, date, name) VALUES ($1, $2, $3)
         ON CONFLICT (region, date) DO UPDATE SET name = EXCLUDED.name, source_id = NULL, hidden = false, moved = false",
    )
    .bind(CALENDAR)
    .bind(f.date)
    .bind(&name)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "holiday.save", &format!("holiday:{}", f.date), serde_json::json!({"name": name, "old_date": old})).await?;
    Ok(Redirect::to("/admin/holidays").into_response())
}

/// Deletes a holiday; an imported one is hidden instead, so the next import does not bring it back.
pub async fn holiday_delete(State(app): State<AppState>, Admin(a): Admin, Path(date): Path<NaiveDate>) -> Result<Response, AppError> {
    sqlx::query("DELETE FROM holidays WHERE region = $1 AND date = $2 AND source_id IS NULL").bind(CALENDAR).bind(date).execute(&app.db).await.map_err(anyhow::Error::from)?;
    sqlx::query("UPDATE holidays SET hidden = true, moved = false WHERE region = $1 AND date = $2").bind(CALENDAR).bind(date).execute(&app.db).await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "holiday.delete", &format!("holiday:{date}"), serde_json::json!({})).await?;
    Ok(Redirect::to("/admin/holidays").into_response())
}

/// Shows hidden (deleted imported) holidays again; ones moved to another date stay hidden.
pub async fn holiday_restore(State(app): State<AppState>, Admin(a): Admin) -> Result<Response, AppError> {
    sqlx::query("UPDATE holidays SET hidden = false WHERE region = $1 AND hidden AND NOT moved").bind(CALENDAR).execute(&app.db).await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "holiday.restore", "holidays", serde_json::json!({})).await?;
    Ok(Redirect::to("/admin/holidays").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn ics_dates_folding_and_rules() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
BEGIN:VEVENT\r\nDTSTART;VALUE=DATE:20260101\r\nDTEND;VALUE=DATE:20260102\r\nSUMMARY:New\r\n  Year\\, Day\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nDTSTART;VALUE=DATE:20261224\r\nDTEND;VALUE=DATE:20261227\r\nSUMMARY:Christmas\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nDTSTART;TZID=Europe/Berlin:20270501T000000\r\nDTEND;TZID=Europe/Berlin:20270501T235959\r\nSUMMARY:Labour Day\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nDTSTART;VALUE=DATE:20201003\r\nRRULE:FREQ=YEARLY\r\nSUMMARY:Unity\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nDTSTART;VALUE=DATE:20200101\r\nRRULE:FREQ=MONTHLY\r\nSUMMARY:Monthly\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nDTSTART;VALUE=DATE:20200601\r\nRRULE:FREQ=YEARLY;UNTIL=20261231\r\nSUMMARY:Until 2026\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nSUMMARY:No start\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nDTSTART;VALUE=DATE:20250101\r\nSUMMARY:Last year\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nDTSTART;VALUE=DATE:20260301\r\nSTATUS:CANCELLED\r\nSUMMARY:Cancelled\r\nEND:VEVENT\r\n\
END:VCALENDAR\r\n";
        let r = parse_ics(ics, &[2026, 2027]);
        assert_eq!(r.skipped, 2, "monthly rule and event without start");
        let e = r.entries;
        assert!(e.contains(&(d(2026, 1, 1), "New Year, Day".into())));
        // all-day DTEND is exclusive: 24., 25., 26.
        assert!(e.contains(&(d(2026, 12, 24), "Christmas".into())));
        assert!(e.contains(&(d(2026, 12, 26), "Christmas".into())));
        assert!(!e.iter().any(|(x, _)| *x == d(2026, 12, 27)));
        assert!(e.contains(&(d(2027, 5, 1), "Labour Day".into())));
        assert!(e.contains(&(d(2026, 10, 3), "Unity".into())));
        assert!(e.contains(&(d(2027, 10, 3), "Unity".into())));
        assert!(e.contains(&(d(2026, 6, 1), "Until 2026".into())));
        assert!(!e.iter().any(|(x, n)| *x == d(2027, 6, 1) || n == "Last year" || n == "Cancelled"));
        assert_eq!(e.len(), 8);
    }

    #[test]
    fn ics_unix_newlines_and_leap_day() {
        let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nDTSTART:20240229\nRRULE:FREQ=YEARLY\nSUMMARY:Leap\nEND:VEVENT\nEND:VCALENDAR\n";
        assert!(parse_ics(ics, &[2026, 2027]).entries.is_empty());
        assert_eq!(parse_ics(ics, &[2028]).entries, vec![(d(2028, 2, 29), "Leap".to_string())]);
    }

    const NAGER: &str = r#"[
      {"date":"2026-01-01","localName":"Neujahr","name":"New Year's Day","countryCode":"DE","fixed":false,"global":true,"counties":null,"launchYear":null,"types":["Public"]},
      {"date":"2026-01-06","localName":"Heilige Drei Könige","name":"Epiphany","countryCode":"DE","fixed":false,"global":false,"counties":["DE-BW","DE-BY","DE-ST"],"launchYear":null,"types":["Public"]},
      {"date":"2026-10-31","localName":"Reformationstag","name":"Reformation Day","countryCode":"DE","fixed":false,"global":false,"counties":["DE-SH","DE-HH"],"launchYear":null,"types":["Public"]},
      {"date":"2026-02-14","localName":"Valentinstag","name":"Valentine's Day","countryCode":"DE","fixed":false,"global":true,"counties":null,"launchYear":null,"types":["Observance"]}
    ]"#;

    #[test]
    fn nager_mapping() {
        let nationwide = map_nager(NAGER, None).unwrap();
        assert_eq!(nationwide, vec![(d(2026, 1, 1), "Neujahr".to_string())]);
        let sh = map_nager(NAGER, Some("DE-SH")).unwrap();
        assert_eq!(sh, vec![(d(2026, 1, 1), "Neujahr".to_string()), (d(2026, 10, 31), "Reformationstag".to_string())]);
        assert!(map_nager("{\"type\":\"error\"}", None).is_err());
    }

    #[test]
    fn source_validation() {
        let s = |kind: &str, c: &str, sub: &str, url: &str| Spec { kind: kind.into(), country: c.into(), subdivision: sub.into(), url: url.into(), file: None };
        assert!(validate(&s("nager", "DE", "", "")).is_ok());
        assert!(validate(&s("nager", "DE", "DE-SH", "")).is_ok());
        assert!(validate(&s("nager", "DE", "AT-9", "")).is_err());
        assert!(validate(&s("nager", "Germany", "", "")).is_err());
        assert!(validate(&s("ical", "", "", "https://example.com/h.ics")).is_ok());
        assert!(validate(&s("ical", "", "", "file:///etc/passwd")).is_err());
        assert!(validate(&s("builtin", "DE", "", "")).is_err());
    }
}
