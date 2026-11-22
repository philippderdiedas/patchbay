// SPDX-License-Identifier: GPL-3.0-or-later
//! Number plan: every internal number has 4 digits (1000–9999) and is registered once in the
//! `numbers` table (triggers keep it in sync and enforce ranges and the cool-down).
//! Per kind a range (or the whole pool) and an allocation strategy come from the environment:
//!
//!   PBX_NUMBERS_PERSON=1000-1999:choose   PBX_NUMBERS_DEVICE=off   PBX_NUMBERS_APP=pool:next
//!
//! Value: `<first>-<last>` or `pool`, optionally `:next|random|choose`; devices also `off`.

use anyhow::{Context, bail};
use sqlx::{FromRow, PgPool};

use crate::AppError;

pub const FIRST: i16 = 1000;
pub const LAST: i16 = 9999;

/// Kinds of internal numbers. Custom apps share the company app policy.
pub const KINDS: [&str; 7] = ["person", "device", "group", "ivr", "custom_app", "app", "company_app"];

#[derive(Clone, Debug, PartialEq)]
pub struct Policy {
    pub first: i16,
    pub last: i16,
    pub strategy: String,
    pub enabled: bool,
    /// PBX_NUMBERS_<KIND>_RESERVED: part of the range only admins assign (never allocated
    /// automatically, not choosable in self-service)
    pub reserved: Option<(i16, i16)>,
}

impl Default for Policy {
    fn default() -> Self {
        Policy { first: FIRST, last: LAST, strategy: "next".into(), enabled: true, reserved: None }
    }
}

impl Policy {
    pub fn choose(&self) -> bool {
        self.strategy == "choose"
    }
    pub fn is_pool(&self) -> bool {
        self.first == FIRST && self.last == LAST
    }
    /// "1000–1999" or "1000–9999 (shared)"
    pub fn range_label(&self) -> String {
        format!("{}–{}", self.first, self.last)
    }
    pub fn is_reserved(&self, n: i16) -> bool {
        self.reserved.is_some_and(|(a, b)| (a..=b).contains(&n))
    }
    /// What users may take: "1100–1999" (range minus a reserved part at its start or end), else
    /// "1000–1999 except 1500–1599"
    pub fn user_range_label(&self) -> String {
        match self.reserved {
            Some((a, b)) if a <= self.first && b < self.last => format!("{}–{}", b + 1, self.last),
            Some((a, b)) if b >= self.last && a > self.first => format!("{}–{}", self.first, a - 1),
            Some((a, b)) => format!("{}–{} (not {a}–{b})", self.first, self.last),
            None => self.range_label(),
        }
    }
}

/// Parses `1000-1999[:strategy]`, `pool[:strategy]` or `off`.
pub fn parse(var: &str, value: &str, may_be_off: bool) -> anyhow::Result<Policy> {
    let v = value.trim();
    if v.is_empty() {
        return Ok(Policy::default());
    }
    if v == "off" {
        anyhow::ensure!(may_be_off, "{var}: only devices can be off");
        return Ok(Policy { enabled: false, ..Policy::default() });
    }
    let (range, strategy) = v.split_once(':').unwrap_or((v, "next"));
    anyhow::ensure!(matches!(strategy, "next" | "random" | "choose"), "{var}: strategy must be next, random or choose");
    let (first, last) = if range == "pool" {
        (FIRST, LAST)
    } else {
        let (a, b) = range.split_once('-').with_context(|| format!("{var}: expected <first>-<last> or pool"))?;
        let a: i16 = a.trim().parse().with_context(|| format!("{var}: invalid number"))?;
        let b: i16 = b.trim().parse().with_context(|| format!("{var}: invalid number"))?;
        (a, b)
    };
    anyhow::ensure!((FIRST..=LAST).contains(&first) && (first..=LAST).contains(&last), "{var}: range must lie within 1000–9999");
    Ok(Policy { first, last, strategy: strategy.into(), enabled: true, reserved: None })
}

/// `<first>-<last>` inside the kind's range, or unset.
fn reserved(var: &str, p: &Policy) -> anyhow::Result<Option<(i16, i16)>> {
    let v = std::env::var(var).unwrap_or_default();
    let v = v.trim();
    if v.is_empty() {
        return Ok(None);
    }
    let (a, b) = v.split_once('-').with_context(|| format!("{var}: expected <first>-<last>"))?;
    let a: i16 = a.trim().parse().with_context(|| format!("{var}: invalid number"))?;
    let b: i16 = b.trim().parse().with_context(|| format!("{var}: invalid number"))?;
    anyhow::ensure!(a <= b && a >= p.first && b <= p.last, "{var}: must lie within {}", p.range_label());
    Ok(Some((a, b)))
}

/// Environment variable of a kind (custom apps use the company app range).
fn var_of(kind: &str) -> &'static str {
    match kind {
        "person" => "PBX_NUMBERS_PERSON",
        "device" => "PBX_NUMBERS_DEVICE",
        "group" => "PBX_NUMBERS_GROUP",
        "ivr" => "PBX_NUMBERS_IVR",
        "app" => "PBX_NUMBERS_APP",
        _ => "PBX_NUMBERS_COMPANY",
    }
}

#[derive(Clone, Debug)]
pub struct NumberPlan {
    pub policies: Vec<(String, Policy)>,
    pub cooldown_days: i32,
    pub change_days: i32,
}

impl NumberPlan {
    pub fn from_env() -> anyhow::Result<Self> {
        let mut policies = vec![];
        for k in KINDS {
            let var = var_of(k);
            let mut p = parse(var, &std::env::var(var).unwrap_or_default(), k == "device")?;
            p.reserved = reserved(&format!("{var}_RESERVED"), &p)?;
            policies.push((k.to_string(), p));
        }
        let days = |var: &str, default: i32| -> anyhow::Result<i32> {
            match std::env::var(var) {
                Ok(v) if !v.trim().is_empty() => v.trim().parse::<i32>().ok().filter(|d| (0..=3650).contains(d)).with_context(|| format!("{var}: days 0–3650")),
                _ => Ok(default),
            }
        };
        Ok(NumberPlan { policies, cooldown_days: days("PBX_NUMBER_COOLDOWN_DAYS", 90)?, change_days: days("PBX_NUMBER_CHANGE_DAYS", 30)? })
    }

    pub fn policy(&self, kind: &str) -> Policy {
        self.policies.iter().find(|(k, _)| k == kind).map(|(_, p)| p.clone()).unwrap_or_default()
    }
}

/// Startup: publish the policies for the database triggers and refuse to start when existing
/// numbers lie outside the configured ranges (a silently broken number plan is worse).
pub async fn startup(db: &PgPool, plan: &NumberPlan) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    for (kind, p) in &plan.policies {
        sqlx::query(
            "INSERT INTO number_policy (kind, first, last, strategy, enabled) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (kind) DO UPDATE SET first = $2, last = $3, strategy = $4, enabled = $5",
)
        .bind(kind)
        .bind(p.first)
        .bind(p.last)
        .bind(&p.strategy)
        .bind(p.enabled)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("INSERT INTO settings (key, value) VALUES ('number_cooldown_days', $1) ON CONFLICT (key) DO UPDATE SET value = $1")
        .bind(plan.cooldown_days.to_string())
        .execute(&mut *tx)
        .await?;
    let outside: Vec<(i16, String)> = sqlx::query_as(
        "SELECT n.number, n.kind FROM numbers n JOIN number_policy p ON p.kind = n.kind
         WHERE n.number NOT BETWEEN p.first AND p.last ORDER BY n.number LIMIT 20",
)
    .fetch_all(&mut *tx)
    .await?;
    if !outside.is_empty() {
        let list: Vec<String> = outside.iter().map(|(n, k)| format!("{n} ({k})")).collect();
        bail!("existing numbers outside the configured PBX_NUMBERS_* ranges: {} – adjust the ranges or renumber first", list.join(", "));
    }
    tx.commit().await?;
    Ok(())
}

/// Free number for a kind: the next or a random one in its range, skipping used and held numbers.
/// None if the kind has numbers switched off (devices) or the range is full.
/// Callers run it inside their insert transaction (see db::create_device), so the number is
/// still free when the row is written.
pub async fn allocate<'e>(db: impl sqlx::PgExecutor<'e>, plan: &NumberPlan, kind: &str) -> Result<Option<i16>, AppError> {
    let random = plan.policy(kind).strategy == "random";
    allocate_with(db, plan, kind, random).await
}

/// Next free number of a kind, or a random free one (self-service "random number").
pub async fn allocate_with<'e>(db: impl sqlx::PgExecutor<'e>, plan: &NumberPlan, kind: &str, random: bool) -> Result<Option<i16>, AppError> {
    let p = plan.policy(kind);
    if !p.enabled {
        return Ok(None);
    }
    let order = if random { "random()" } else { "n" };
    let n: Option<i32> = sqlx::query_scalar(&format!(
        "SELECT n FROM generate_series($1::int, $2::int) n
         WHERE NOT EXISTS (SELECT 1 FROM numbers WHERE number = n)
           AND NOT EXISTS (SELECT 1 FROM number_holds h WHERE h.number = n AND h.released_at > now() - make_interval(days => $3))
           AND NOT (n BETWEEN $4 AND $5)
         ORDER BY {order} LIMIT 1"
))
    .bind(p.first as i32)
    .bind(p.last as i32)
    .bind(plan.cooldown_days)
    .bind(p.reserved.map(|r| r.0 as i32).unwrap_or(0))
    .bind(p.reserved.map(|r| r.1 as i32).unwrap_or(-1))
    .fetch_optional(db)
    .await
    .map_err(anyhow::Error::from)?;
    match n {
        Some(n) => Ok(Some(n as i16)),
        None => Err(AppError::BadRequest(format!("No free number left in {}.", p.range_label()))),
    }
}

/// CLI `patchbay-web next-number <kind>` (pbxctl): prints the next free number of a kind; fails when the
/// range is full or the kind has numbers switched off.
pub async fn cli_next_number(kind: Option<String>) -> anyhow::Result<()> {
    let kind = kind.filter(|k| KINDS.contains(&k.as_str())).with_context(|| format!("usage: patchbay-web next-number <{}>", KINDS.join("|")))?;
    let plan = NumberPlan::from_env()?;
    let db = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&crate::config::database_url()?)
        .await
        .context("Database unreachable")?;
    match allocate(&db, &plan, &kind).await {
        Ok(Some(n)) => {
            println!("{n}");
            Ok(())
        }
        Ok(None) => bail!("numbers of kind {kind} are switched off"),
        Err(AppError::BadRequest(m)) => bail!(m),
        Err(e) => bail!("{e:?}"),
    }
}

/// Like `check`, for a number a user picks themselves: not from the reserved part of the range.
pub async fn check_self(db: &PgPool, plan: &NumberPlan, kind: &str, raw: &str, own_ref: Option<i32>) -> Result<i16, AppError> {
    let p = plan.policy(kind);
    if let Ok(n) = raw.trim().parse::<i16>() {
        if p.is_reserved(n) {
            return Err(AppError::BadRequest(format!("Numbers {} are reserved; free for you: {}.", {
                let (a, b) = p.reserved.unwrap_or_default();
                format!("{a}–{b}")
            }, p.user_range_label())));
        }
    }
    check(db, plan, kind, raw, own_ref).await
}

/// Checks a number chosen in a form (own_ref: the entity that may keep or re-take it).
pub async fn check(db: &PgPool, plan: &NumberPlan, kind: &str, raw: &str, own_ref: Option<i32>) -> Result<i16, AppError> {
    let p = plan.policy(kind);
    let n: i16 = raw
        .trim()
        .parse()
        .ok()
        .filter(|n| (FIRST..=LAST).contains(n))
        .ok_or_else(|| AppError::BadRequest("Internal numbers have 4 digits (1000–9999).".into()))?;
    if n < p.first || n > p.last {
        return Err(AppError::BadRequest(format!("The number must be in the range {}.", p.range_label())));
    }
    let used: Option<(String, i32)> = sqlx::query_as("SELECT kind, ref_id FROM numbers WHERE number = $1")
        .bind(n)
        .fetch_optional(db)
        .await
        .map_err(anyhow::Error::from)?;
    if let Some((k, r)) = used {
        if !(k == kind && Some(r) == own_ref) {
            return Err(AppError::BadRequest(format!("Number {n} is already in use.")));
        }
    }
    let held: Option<String> = sqlx::query_scalar(
        "SELECT to_char(released_at + make_interval(days => $2), 'DD.MM.YYYY') FROM number_holds
         WHERE number = $1 AND released_at > now() - make_interval(days => $2) AND NOT (kind = $3 AND ref_id = $4)",
)
    .bind(n)
    .bind(plan.cooldown_days)
    .bind(kind)
    .bind(own_ref.unwrap_or(0))
    .fetch_optional(db)
    .await
    .map_err(anyhow::Error::from)?;
    if let Some(until) = held {
        return Err(AppError::BadRequest(format!("Number {n} was released recently and is blocked until {until}.")));
    }
    Ok(n)
}

/// A form value: empty = allocate automatically, otherwise check the chosen number.
pub async fn from_form(db: &PgPool, plan: &NumberPlan, kind: &str, raw: &str, own_ref: Option<i32>) -> Result<i16, AppError> {
    if raw.trim().is_empty() {
        return allocate(db, plan, kind).await?.ok_or_else(|| AppError::BadRequest("Numbers are switched off for this kind.".into()));
    }
    check(db, plan, kind, raw, own_ref).await
}

/// Database errors raised by the number triggers as a readable message.
pub fn db_error(e: sqlx::Error) -> AppError {
    match &e {
        sqlx::Error::Database(d) if d.message().starts_with("Number ") => AppError::BadRequest(d.message().to_string()),
        sqlx::Error::Database(d) if d.code().as_deref() == Some("23505") => AppError::BadRequest("The number is already in use.".into()),
        _ => AppError::Internal(e.into()),
    }
}

// --- Overview (admin number plan page, help page) ---------------------------------------------

#[derive(FromRow)]
pub struct Usage {
    pub kind: String,
    pub used: i64,
}

#[derive(FromRow)]
pub struct Hold {
    pub number: i16,
    pub kind: String,
    pub released: String,
    pub until: String,
}

pub async fn usage(db: &PgPool) -> Result<Vec<Usage>, AppError> {
    Ok(sqlx::query_as("SELECT kind, count(*) AS used FROM numbers GROUP BY kind").fetch_all(db).await.map_err(anyhow::Error::from)?)
}

pub async fn holds(db: &PgPool, plan: &NumberPlan) -> Result<Vec<Hold>, AppError> {
    Ok(sqlx::query_as(
        "SELECT number, kind, to_char(released_at, 'DD.MM.YYYY') AS released,
                to_char(released_at + make_interval(days => $1), 'DD.MM.YYYY') AS until
         FROM number_holds WHERE released_at > now() - make_interval(days => $1) ORDER BY number",
)
    .bind(plan.cooldown_days)
    .fetch_all(db)
    .await
    .map_err(anyhow::Error::from)?)
}

/// What an internal number is: (kind, display name)
pub async fn lookup(db: &PgPool, n: i16) -> Result<Option<(String, String)>, AppError> {
    Ok(sqlx::query_as(
        "SELECT n.kind, COALESCE(
            CASE n.kind
              WHEN 'person' THEN (SELECT display_name FROM users WHERE id = n.ref_id)
              WHEN 'device' THEN (SELECT u.display_name || ' · ' || d.label FROM devices d JOIN users u ON u.id = d.user_id WHERE d.id = n.ref_id)
              WHEN 'group' THEN (SELECT name FROM groups WHERE id = n.ref_id)
              WHEN 'ivr' THEN (SELECT name FROM ivrs WHERE id = n.ref_id)
              WHEN 'custom_app' THEN (SELECT name FROM custom_apps WHERE id = n.ref_id)
              ELSE (SELECT name FROM apps WHERE id = n.ref_id)
            END, '') AS name
         FROM numbers n WHERE n.number = $1",
)
    .bind(n)
    .fetch_optional(db)
    .await
    .map_err(anyhow::Error::from)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_policies() {
        assert_eq!(parse("X", "", false).unwrap(), Policy::default());
        assert_eq!(parse("X", "1000-1999", false).unwrap(), Policy { first: 1000, last: 1999, strategy: "next".into(), enabled: true, reserved: None });
        assert_eq!(parse("X", "pool:choose", false).unwrap(), Policy { strategy: "choose".into(), ..Policy::default() });
        assert_eq!(parse("X", "2000-3999:random", false).unwrap().strategy, "random");
        assert!(!parse("X", "off", true).unwrap().enabled);
        assert!(parse("X", "off", false).is_err());
        assert!(parse("X", "999-1999", false).is_err());
        assert!(parse("X", "2000-1000", false).is_err());
        assert!(parse("X", "1000-1999:best", false).is_err());
        assert!(parse("X", "10-99", false).is_err());
    }
}

// --- Admin page ----------------------------------------------------------------------------------

use askama::Template;
use axum::{
    extract::{Path, State},
    response::{IntoResponse, Redirect, Response},
};

use crate::{AppState, auth::Admin, ui::{Ctx, render}};

pub struct KindRow {
    pub label: &'static str,
    pub policy: Policy,
    pub used: i64,
    pub size: i64,
}

#[derive(FromRow)]
pub struct Assigned {
    pub number: i16,
    pub kind: String,
    pub name: String,
}

/// English label of a kind (translated in templates)
pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        "person" => "Person",
        "device" => "Device",
        "group" => "Call group",
        "ivr" => "Voice menu",
        "custom_app" => "Custom app",
        "app" => "Personal app",
        "company_app" => "Organization app",
        _ => "",
    }
}

impl Assigned {
    pub fn kind_label(&self) -> &'static str {
        kind_label(&self.kind)
    }
}

#[derive(Template)]
#[template(path = "admin_numberplan.html")]
struct PlanPage {
    c: Ctx,
    kinds: Vec<KindRow>,
    assigned: Vec<Assigned>,
    holds: Vec<Hold>,
    cooldown_days: i32,
    change_days: i32,
}

impl Hold {
    pub fn kind_label(&self) -> &'static str {
        kind_label(&self.kind)
    }
}

pub async fn plan_page(State(app): State<AppState>, Admin(a): Admin) -> Result<Response, AppError> {
    let plan = &app.cfg.numbers;
    let usage = usage(&app.db).await?;
    let kinds = KINDS
        .iter()
        .map(|k| {
            let p = plan.policy(k);
            KindRow {
                label: kind_label(k),
                used: usage.iter().find(|u| u.kind == *k).map(|u| u.used).unwrap_or(0),
                size: (p.last - p.first + 1) as i64,
                policy: p,
            }
        })
        .collect();
    let assigned: Vec<Assigned> = sqlx::query_as(
        "SELECT n.number, n.kind, COALESCE(
            CASE n.kind
              WHEN 'person' THEN (SELECT display_name FROM users WHERE id = n.ref_id)
              WHEN 'device' THEN (SELECT u.display_name || ' · ' || d.label FROM devices d JOIN users u ON u.id = d.user_id WHERE d.id = n.ref_id)
              WHEN 'group' THEN (SELECT name FROM groups WHERE id = n.ref_id)
              WHEN 'ivr' THEN (SELECT name FROM ivrs WHERE id = n.ref_id)
              WHEN 'custom_app' THEN (SELECT name FROM custom_apps WHERE id = n.ref_id)
              ELSE (SELECT a.name || COALESCE(' · ' || u.display_name, '') FROM apps a LEFT JOIN users u ON u.id = a.owner_user_id WHERE a.id = n.ref_id)
            END, '') AS name
         FROM numbers n ORDER BY n.number",
)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    let holds = holds(&app.db, plan).await?;
    let page = PlanPage { c: Ctx::new(&app, &a).section("numberplan"), kinds, assigned, holds, cooldown_days: plan.cooldown_days, change_days: plan.change_days };
    render(page)
}

/// Releases a blocked number before its cool-down ends.
pub async fn release(State(app): State<AppState>, Admin(a): Admin, Path(number): Path<i16>) -> Result<Response, AppError> {
    sqlx::query("DELETE FROM number_holds WHERE number = $1").bind(number).execute(&app.db).await.map_err(anyhow::Error::from)?;
    crate::db::audit(&app.db, &a.session.sub, "number.release", &format!("number:{number}"), serde_json::json!({})).await?;
    Ok(Redirect::to("/admin/numberplan#holds").into_response())
}

#[cfg(test)]
mod reserved_tests {
    use super::*;

    #[test]
    fn labels() {
        let p = Policy { first: 1000, last: 1999, reserved: Some((1000, 1099)), ..Policy::default() };
        assert_eq!(p.user_range_label(), "1100–1999");
        assert!(p.is_reserved(1050) && !p.is_reserved(1100));
        let q = Policy { first: 1000, last: 1999, reserved: Some((1500, 1599)), ..Policy::default() };
        assert_eq!(q.user_range_label(), "1000–1999 (not 1500–1599)");
    }
}
