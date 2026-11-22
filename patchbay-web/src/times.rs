// SPDX-License-Identifier: GPL-3.0-or-later
//! Time profiles (opening hours, holidays, closing days) and the per-number routing that uses them.

use askama::Template;
use axum::{
    Form,
    extract::{Path, State},
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::Form as MultiForm;
use chrono::{NaiveDate, NaiveTime};
use serde::Deserialize;
use sqlx::{FromRow, PgPool};

use crate::{
    AppError, AppState,
    admin::{TargetInput, save_target},
    auth::Admin,
    db,
    ui::{Ctx, TargetOptions, TargetSel, render},
};

// --- Startup ----------------------------------------------------------------------------------

/// Applies the site time zone (PBX_TIMEZONE) to all time profiles and starts the daily re-import of
/// holiday sources (holidays.rs).
pub fn spawn_holidays(db: PgPool, cfg: &crate::config::Config) {
    let tz = cfg.timezone.clone();
    let nager = cfg.holiday_nager_url.clone();
    tokio::spawn(async move {
        if let Err(e) = sqlx::query("UPDATE time_profiles SET timezone = $1 WHERE timezone <> $1").bind(&tz).execute(&db).await {
            tracing::warn!("time profile time zone: {e:#}");
        }
        crate::holidays::spawn(db, nager);
    });
}

// --- Time profiles -----------------------------------------------------------------------

#[derive(FromRow)]
pub struct ProfileRow {
    id: i32,
    name: String,
    holiday_region: String,
    state: String,
    rules: i64,
    used: i64,
}

#[derive(FromRow)]
pub struct RuleRow {
    id: i32,
    weekdays: i16,
    from_time: NaiveTime,
    to_time: NaiveTime,
}

const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

impl RuleRow {
    fn days(&self) -> Vec<&'static str> {
        (0..7).filter(|i| self.weekdays >> i & 1 == 1).map(|i| DAYS[i as usize]).collect()
    }
    fn from(&self) -> String {
        self.from_time.format("%H:%M").to_string()
    }
    fn to(&self) -> String {
        self.to_time.format("%H:%M").to_string()
    }
}

#[derive(FromRow)]
pub struct DayRow {
    date: NaiveDate,
    name: String,
}

impl DayRow {
    fn date_str(&self) -> String {
        self.date.format("%d.%m.%Y").to_string()
    }
    fn iso(&self) -> String {
        self.date.format("%Y-%m-%d").to_string()
    }
}

#[derive(Template)]
#[template(path = "admin_times.html")]
struct ProfilesPage {
    c: Ctx,
    profiles: Vec<ProfileRow>,
}

#[derive(Template)]
#[template(path = "admin_time.html")]
struct ProfilePage {
    c: Ctx,
    p: ProfileRow,
    rules: Vec<RuleRow>,
    closures: Vec<DayRow>,
    holidays: Vec<DayRow>,
    days: [&'static str; 7],
    timezone: String,
}

const PROFILE_SELECT: &str = "SELECT tp.id, tp.name, tp.holiday_region, time_profile_state(tp.id) AS state,
        (SELECT count(*) FROM time_rules r WHERE r.profile_id = tp.id) AS rules,
        (SELECT count(*) FROM inbound_rules i WHERE i.time_profile_id = tp.id) AS used
    FROM time_profiles tp WHERE tp.deleted_at IS NULL";

pub async fn profiles(State(app): State<AppState>, Admin(a): Admin) -> Result<Response, AppError> {
    let profiles = sqlx::query_as(&format!("{PROFILE_SELECT} ORDER BY tp.name"))
        .fetch_all(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    render(ProfilesPage { c: Ctx::new(&app, &a).section("times"), profiles })
}

#[derive(Deserialize)]
pub struct NewProfile {
    name: String,
}

pub async fn profile_create(State(app): State<AppState>, Admin(a): Admin, Form(f): Form<NewProfile>) -> Result<Response, AppError> {
    let name: String = f.name.trim().chars().take(40).collect();
    if name.is_empty() {
        return Err(AppError::BadRequest("Name missing.".into()));
    }
    let id: i32 = sqlx::query_scalar("INSERT INTO time_profiles (name, timezone, holiday_region) VALUES ($1, $2, $3) RETURNING id")
        .bind(&name)
        .bind(&app.cfg.timezone)
        .bind(crate::holidays::CALENDAR)
        .fetch_one(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "time.add", &format!("profile:{id}"), serde_json::json!({"name": name})).await?;
    Ok(Redirect::to(&format!("/admin/times/{id}")).into_response())
}

async fn load_profile(db: &PgPool, id: i32) -> Result<ProfileRow, AppError> {
    sqlx::query_as(&format!("{PROFILE_SELECT} AND tp.id = $1"))
        .bind(id)
        .fetch_optional(db)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or(AppError::NotFound)
}

pub async fn profile(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let p = load_profile(&app.db, id).await?;
    let rules = sqlx::query_as("SELECT id, weekdays, from_time, to_time FROM time_rules WHERE profile_id = $1 ORDER BY from_time, id")
        .bind(id)
        .fetch_all(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let closures = sqlx::query_as("SELECT date, name FROM profile_closures WHERE profile_id = $1 AND date >= current_date - 7 ORDER BY date")
        .bind(id)
        .fetch_all(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let holidays = sqlx::query_as(
        "SELECT date, name FROM holidays WHERE region = $1 AND NOT hidden AND date >= current_date AND date < current_date + 365 ORDER BY date",
    )
    .bind(crate::holidays::CALENDAR)
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    render(ProfilePage {
        c: Ctx::new(&app, &a).section("times"),
        p,
        rules,
        closures,
        holidays,
        days: DAYS,
        timezone: app.cfg.timezone.clone(),
    })
}

#[derive(Deserialize)]
pub struct ProfileForm {
    name: String,
    holiday_region: String,
}

pub async fn profile_save(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>, Form(f): Form<ProfileForm>) -> Result<Response, AppError> {
    // public holidays on (the site calendar) or off
    let region = if f.holiday_region == crate::holidays::CALENDAR { crate::holidays::CALENDAR } else { "none" };
    sqlx::query("UPDATE time_profiles SET name = $2, holiday_region = $3 WHERE id = $1")
        .bind(id)
        .bind(f.name.trim())
        .bind(region)
        .execute(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "time.edit", &format!("profile:{id}"), serde_json::json!({})).await?;
    Ok(Redirect::to(&format!("/admin/times/{id}")).into_response())
}

#[derive(Deserialize)]
pub struct RuleForm {
    #[serde(default)]
    days: Vec<u8>,
    from: String,
    to: String,
}

pub async fn rule_add(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>, MultiForm(f): MultiForm<RuleForm>) -> Result<Response, AppError> {
    load_profile(&app.db, id).await?;
    let mask: i16 = f.days.iter().filter(|d| **d < 7).fold(0, |m, d| m | (1 << d));
    if mask == 0 {
        return Err(AppError::BadRequest("Select at least one weekday.".into()));
    }
    let parse = |s: &str| NaiveTime::parse_from_str(s, "%H:%M").map_err(|_| AppError::BadRequest("Time format HH:MM.".into()));
    let (from, to) = (parse(&f.from)?, parse(&f.to)?);
    if to <= from {
        return Err(AppError::BadRequest("“To” must be after “from” (use two rules for times past midnight).".into()));
    }
    sqlx::query("INSERT INTO time_rules (profile_id, weekdays, from_time, to_time) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(mask)
        .bind(from)
        .bind(to)
        .execute(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "time.rule_add", &format!("profile:{id}"), serde_json::json!({"mask": mask, "from": f.from, "to": f.to})).await?;
    Ok(Redirect::to(&format!("/admin/times/{id}")).into_response())
}

pub async fn rule_delete(State(app): State<AppState>, Admin(a): Admin, Path((id, rid)): Path<(i32, i32)>) -> Result<Response, AppError> {
    sqlx::query("DELETE FROM time_rules WHERE profile_id = $1 AND id = $2")
        .bind(id)
        .bind(rid)
        .execute(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "time.rule_delete", &format!("profile:{id}"), serde_json::json!({"rule": rid})).await?;
    Ok(Redirect::to(&format!("/admin/times/{id}")).into_response())
}

#[derive(Deserialize)]
pub struct ClosureForm {
    date: NaiveDate,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    name: String,
}

/// Adds one closing day or a range (e.g. company holidays).
pub async fn closure_add(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>, Form(f): Form<ClosureForm>) -> Result<Response, AppError> {
    load_profile(&app.db, id).await?;
    let to = match f.to.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| AppError::BadRequest("Invalid date.".into()))?,
        None => f.date,
    };
    if to < f.date || (to - f.date).num_days() > 60 {
        return Err(AppError::BadRequest("Range: at most 60 days, end not before start.".into()));
    }
    sqlx::query(
        "INSERT INTO profile_closures (profile_id, date, name)
         SELECT $1, d::date, $4 FROM generate_series($2::date, $3::date, '1 day') d
         ON CONFLICT (profile_id, date) DO UPDATE SET name = EXCLUDED.name",
    )
    .bind(id)
    .bind(f.date)
    .bind(to)
    .bind(f.name.trim())
    .execute(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "time.closure_add", &format!("profile:{id}"), serde_json::json!({"from": f.date, "to": to})).await?;
    Ok(Redirect::to(&format!("/admin/times/{id}")).into_response())
}

pub async fn closure_delete(State(app): State<AppState>, Admin(a): Admin, Path((id, date)): Path<(i32, NaiveDate)>) -> Result<Response, AppError> {
    sqlx::query("DELETE FROM profile_closures WHERE profile_id = $1 AND date = $2")
        .bind(id)
        .bind(date)
        .execute(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "time.closure_delete", &format!("profile:{id}"), serde_json::json!({"date": date})).await?;
    Ok(Redirect::to(&format!("/admin/times/{id}")).into_response())
}

pub async fn profile_delete(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let p = load_profile(&app.db, id).await?;
    if p.used > 0 {
        return Err(AppError::BadRequest("The profile is still used by a number.".into()));
    }
    sqlx::query("UPDATE time_profiles SET deleted_at = now() WHERE id = $1").bind(id).execute(&app.db).await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, &a.session.sub, "time.delete", &format!("profile:{id}"), serde_json::json!({})).await?;
    Ok(Redirect::to("/admin/times").into_response())
}

// --- Number routing (profile + open/closed/holiday targets) ------------------------------

#[derive(FromRow)]
pub struct NumberRow {
    id: i32,
    e164: String,
    label: String,
    time_profile_id: Option<i32>,
    open_kind: Option<String>,
    open_value: Option<String>,
    closed_kind: Option<String>,
    closed_value: Option<String>,
    holiday_kind: Option<String>,
    holiday_value: Option<String>,
}

impl NumberRow {
    fn profile_is(&self, id: &i32) -> bool {
        self.time_profile_id == Some(*id)
    }
}

#[derive(Template)]
#[template(path = "admin_number.html")]
struct NumberPage {
    c: Ctx,
    n: NumberRow,
    profiles: Vec<(i32, String)>,
    opts: TargetOptions,
    open: TargetSel,
    closed: TargetSel,
    holiday: TargetSel,
}

fn target_cols(alias: &str, col: &str) -> String {
    format!(
        "{a}.id AS {col}_id, {a}.kind::text AS {col}_kind,
         COALESCE((SELECT extension::text FROM users WHERE id = {a}.user_id), (SELECT number::text FROM groups WHERE id = {a}.group_id),
                  {a}.ivr_id::text, {a}.sound_id::text, {a}.external_e164) AS {col}_value",
        a = alias
    )
}

pub async fn number(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let n: NumberRow = sqlx::query_as(&format!(
        "SELECT d.id, d.e164, d.label, ir.time_profile_id, {}, {}, {}
         FROM dids d LEFT JOIN inbound_rules ir ON ir.did_id = d.id
         LEFT JOIN targets o ON o.id = ir.target_open LEFT JOIN targets c ON c.id = ir.target_closed
         LEFT JOIN targets h ON h.id = ir.target_holiday
         WHERE d.id = $1",
        target_cols("o", "open"),
        target_cols("c", "closed"),
        target_cols("h", "holiday")
    ))
    .bind(id)
    .fetch_optional(&app.db)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or(AppError::NotFound)?;
    let profiles = sqlx::query_as("SELECT id, name FROM time_profiles WHERE deleted_at IS NULL ORDER BY name")
        .fetch_all(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let open = TargetSel::new("open_", n.open_kind.clone(), n.open_value.clone(), "no target");
    let closed = TargetSel::new("closed_", n.closed_kind.clone(), n.closed_value.clone(), "same as open");
    let holiday = TargetSel::new("holiday_", n.holiday_kind.clone(), n.holiday_value.clone(), "same as closed");
    render(NumberPage { c: Ctx::new(&app, &a).section("numbers"), n, profiles, opts: TargetOptions::load(&app.db).await?, open, closed, holiday })
}

#[derive(Deserialize)]
pub struct NumberForm {
    time_profile_id: String,
    open_kind: String,
    #[serde(default)]
    open_value: String,
    closed_kind: String,
    #[serde(default)]
    closed_value: String,
    holiday_kind: String,
    #[serde(default)]
    holiday_value: String,
}

pub async fn number_save(State(app): State<AppState>, Admin(a): Admin, Path(id): Path<i32>, Form(f): Form<NumberForm>) -> Result<Response, AppError> {
    let ex: Option<(Option<i32>, Option<i32>, Option<i32>)> =
        sqlx::query_as("SELECT target_open, target_closed, target_holiday FROM inbound_rules WHERE did_id = $1")
            .bind(id)
            .fetch_optional(&app.db)
            .await
            .map_err(anyhow::Error::from)?;
    let (eo, ec, eh) = ex.unwrap_or((None, None, None));
    let open = save_target(&app.db, eo, &TargetInput { kind: f.open_kind, value: f.open_value }).await?;
    let closed = save_target(&app.db, ec, &TargetInput { kind: f.closed_kind, value: f.closed_value }).await?;
    let holiday = save_target(&app.db, eh, &TargetInput { kind: f.holiday_kind, value: f.holiday_value }).await?;
    let profile = f.time_profile_id.parse::<i32>().ok();
    match open {
        Some(open) => {
            sqlx::query(
                "INSERT INTO inbound_rules (did_id, time_profile_id, target_open, target_closed, target_holiday)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (did_id) DO UPDATE SET time_profile_id = EXCLUDED.time_profile_id, target_open = EXCLUDED.target_open,
                        target_closed = EXCLUDED.target_closed, target_holiday = EXCLUDED.target_holiday",
            )
            .bind(id)
            .bind(profile)
            .bind(open)
            .bind(closed)
            .bind(holiday)
            .execute(&app.db)
            .await
            .map_err(anyhow::Error::from)?;
        }
        None => {
            sqlx::query("DELETE FROM inbound_rules WHERE did_id = $1").bind(id).execute(&app.db).await.map_err(anyhow::Error::from)?;
        }
    }
    db::audit(&app.db, &a.session.sub, "did.routing", &format!("did-id:{id}"), serde_json::json!({"profile": profile})).await?;
    Ok(Redirect::to("/admin/numbers").into_response())
}
