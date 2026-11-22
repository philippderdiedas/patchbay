// SPDX-License-Identifier: GPL-3.0-or-later
//! Help page: dialing, feature codes and in-call keys, plus the user's own numbers (printable).

use askama::Template;
use axum::{
    extract::State,
    response::Response,
};
use sqlx::FromRow;

use crate::{AppError, AppState, auth::CurrentUser, i18n::Tr, ui::{Ctx, render}};

/// A feature code as shown on the help page: `<prefix>` [+ argument] [`<suffix>`].
pub struct FeatureCode {
    pub prefix: &'static str,
    /// English label of the argument (e.g. "extension"), None for codes dialled as they are
    pub arg: Option<&'static str>,
    pub suffix: &'static str,
    /// English description (help page, note in the "Services" phone book)
    pub help: &'static str,
    /// English short name for the "Services" phone book (codes without an argument)
    pub name: &'static str,
}

/// The feature codes of the dialplan (`[feature-codes]` in extensions.conf). Single source for
/// the help page and the "Services" phone book (web and CardDAV).
pub const FEATURE_CODES: &[FeatureCode] = &[
    FeatureCode { prefix: "*8", arg: None, suffix: "", help: "Pick up a call ringing in your pickup group", name: "Call pickup" },
    FeatureCode {
        prefix: "*8",
        arg: Some("extension"),
        suffix: "",
        help: "Pick up a ringing call of a colleague in your pickup group, e.g. *81020",
        name: "",
    },
    FeatureCode { prefix: "*761", arg: None, suffix: "", help: "Do not disturb: on", name: "Do not disturb on" },
    FeatureCode { prefix: "*760", arg: None, suffix: "", help: "Do not disturb: off", name: "Do not disturb off" },
    FeatureCode { prefix: "*21*", arg: Some("number"), suffix: "#", help: "Forward all calls immediately, e.g. *21*01761234567#", name: "" },
    FeatureCode { prefix: "#21#", arg: None, suffix: "", help: "Cancel the forwarding", name: "Forwarding off" },
    FeatureCode { prefix: "*97", arg: None, suffix: "", help: "Voicemail: listen to your messages", name: "Voicemail" },
    FeatureCode { prefix: "*43", arg: None, suffix: "", help: "Echo test: you hear yourself (checks audio and delay)", name: "Echo test" },
];

/// An entry of the "Services" phone book: something every user can dial without arguments.
#[derive(Clone, Debug, PartialEq)]
pub struct Service {
    /// stable id (CardDAV UID and resource name), derived from the number (numbers are unique)
    pub uid: String,
    /// what to dial, e.g. "8000", "*97", "#21#", "112"
    pub number: String,
    /// display name in the user's language, e.g. "Voicemail (*97)", "Sales"
    pub name: String,
    /// kind in the user's language, e.g. "Call group", "Feature code"
    pub kind: String,
    /// description in the user's language (feature codes)
    pub note: Option<String>,
}

impl Service {
    fn new(number: &str, name: String, kind: &str, note: Option<&str>, t: Tr) -> Self {
        Service {
            uid: format!("svc-{}", number.replace('*', "s").replace('#', "h")),
            number: number.to_string(),
            name,
            kind: t.s(kind).to_string(),
            note: note.map(|n| t.s(n).to_string()),
        }
    }
    /// Internal 4-digit number: click-to-call works (feature codes need the caller's device,
    /// '#' does not survive click-to-call, emergency numbers are dialled on purpose only).
    pub fn internal(&self) -> bool {
        self.number.len() == 4 && self.number.chars().all(|c| c.is_ascii_digit())
    }
    /// tel: URI ('#' percent-encoded)
    pub fn tel(&self) -> String {
        format!("tel:{}", self.number.replace('#', "%23"))
    }
}

/// Emergency numbers and feature codes without arguments (no database needed).
pub fn fixed_services(emergency: &[String], t: Tr) -> Vec<Service> {
    let mut out: Vec<Service> =
        emergency.iter().map(|n| Service::new(n, format!("{} {n}", t.s("Emergency")), "Emergency", None, t)).collect();
    for f in FEATURE_CODES.iter().filter(|f| f.arg.is_none()) {
        out.push(Service::new(f.prefix, format!("{} ({})", t.s(f.name), f.prefix), "Feature code", Some(f.help), t));
    }
    out
}

/// Company-wide numbers: (call groups, voice menus, company apps incl. enabled custom apps).
/// Same visibility as the help page's "Organization numbers".
pub async fn company_numbers(db: &sqlx::PgPool) -> anyhow::Result<(Vec<Numbered>, Vec<Numbered>, Vec<Numbered>)> {
    let groups = sqlx::query_as("SELECT number::text AS number, name, '' AS kind FROM groups WHERE deleted_at IS NULL ORDER BY number").fetch_all(db).await?;
    let ivrs = sqlx::query_as("SELECT number::text AS number, name, '' AS kind FROM ivrs WHERE deleted_at IS NULL AND number IS NOT NULL ORDER BY number")
        .fetch_all(db)
        .await?;
    let apps = sqlx::query_as(
        "SELECT number::text AS number, name, kind FROM apps WHERE owner_user_id IS NULL AND deleted_at IS NULL AND enabled
         UNION ALL SELECT number::text, name, 'custom' FROM custom_apps WHERE enabled AND deleted_at IS NULL ORDER BY 1",
    )
    .fetch_all(db)
    .await?;
    Ok((groups, ivrs, apps))
}

/// The whole "Services" phone book in the user's language: emergency numbers, feature codes,
/// call groups, voice menus and company apps.
pub async fn services(app: &AppState, t: Tr) -> anyhow::Result<Vec<Service>> {
    let mut out = fixed_services(&app.cfg.emergency_numbers, t);
    let (groups, ivrs, apps) = company_numbers(&app.db).await?;
    out.extend(groups.into_iter().map(|g| Service::new(&g.number, g.name, "Call group", None, t)));
    out.extend(ivrs.into_iter().map(|i| Service::new(&i.number, i.name, "Voice menu", None, t)));
    out.extend(apps.into_iter().map(|a| {
        Service::new(&a.number, a.name, crate::apps::kind_label(&a.kind), None, t)
    }));
    Ok(out)
}

#[derive(FromRow)]
pub struct Numbered {
    pub number: String,
    pub name: String,
    pub kind: String,
}


#[derive(Template)]
#[template(path = "help.html")]
struct HelpPage {
    c: Ctx,
    me: Option<(String, String)>,
    pickup_group: Option<String>,
    devices: Vec<Numbered>,
    my_groups: Vec<Numbered>,
    my_apps: Vec<Numbered>,
    groups: Vec<Numbered>,
    ivrs: Vec<Numbered>,
    company_apps: Vec<Numbered>,
    sites: Vec<Numbered>,
    /// number plan: (range, kinds) – kinds sharing a range are listed together
    ranges: Vec<(String, Vec<&'static str>)>,
    /// PBX_EMERGENCY_NUMBERS
    emergency: Vec<String>,
    codes: &'static [FeatureCode],
}

/// Ranges of the number plan for the "Dialing" table, kinds with the same range merged.
fn plan_ranges(plan: &crate::numbers::NumberPlan) -> Vec<(String, Vec<&'static str>)> {
    let mut out: Vec<(i16, String, Vec<&'static str>)> = vec![];
    for k in crate::numbers::KINDS {
        let p = plan.policy(k);
        if !p.enabled || k == "custom_app" {
            continue;
        }
        let label = crate::numbers::kind_label(k);
        match out.iter_mut().find(|(_, r, _)| *r == p.range_label()) {
            Some(e) => e.2.push(label),
            None => out.push((p.first, p.range_label(), vec![label])),
        }
    }
    out.sort_by_key(|e| e.0);
    out.into_iter().map(|(_, r, k)| (r, k)).collect()
}

async fn list(db: &sqlx::PgPool, sql: &str, uid: i32) -> Result<Vec<Numbered>, AppError> {
    Ok(sqlx::query_as(sql).bind(uid).fetch_all(db).await.map_err(anyhow::Error::from)?)
}

pub async fn help(State(app): State<AppState>, u: CurrentUser) -> Result<Response, AppError> {
    let uid = u.session.user_id.unwrap_or(0);
    let db = &app.db;
    let me: Option<(String, String, Option<String>)> =
        sqlx::query_as("SELECT extension::text, display_name, pickup_group FROM users WHERE id = $1 AND deleted_at IS NULL")
            .bind(uid)
            .fetch_optional(db)
            .await
            .map_err(anyhow::Error::from)?;
    let devices = list(db, "SELECT number::text AS number, label AS name, type::text AS kind FROM active_devices WHERE user_id = $1 AND number IS NOT NULL ORDER BY number", uid).await?;
    let my_groups = list(
        db,
        "SELECT g.number::text AS number, g.name, '' AS kind FROM group_members m JOIN groups g ON g.id = m.group_id
         WHERE m.user_id = $1 AND g.deleted_at IS NULL ORDER BY g.number",
        uid,
    )
    .await?;
    let my_apps = list(db, "SELECT number::text AS number, name, kind FROM apps WHERE owner_user_id = $1 AND deleted_at IS NULL AND enabled ORDER BY number", uid).await?;
    let (groups, ivrs, company_apps) = company_numbers(db).await?;
    let sites = list(
        db,
        "SELECT site_prefix::text AS number, name, '' AS kind FROM trunks
         WHERE deleted_at IS NULL AND enabled AND site_prefix IS NOT NULL AND $1 = $1 ORDER BY site_prefix",
        uid,
    )
    .await?;
    let (me, pickup_group) = match me {
        Some((e, n, p)) => (Some((e, n)), p.filter(|p| !p.is_empty())),
        None => (None, None),
    };
    let ranges = plan_ranges(&app.cfg.numbers);
    let emergency = app.cfg.emergency_numbers.clone();
    let page = HelpPage { c: Ctx::new(&app, &u), me, pickup_group, devices, my_groups, my_apps, groups, ivrs, company_apps, sites, ranges, emergency, codes: FEATURE_CODES };
    render(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Lang;

    #[test]
    fn services_have_only_dialable_codes() {
        let s = fixed_services(&["110".into(), "112".into()], Tr(Lang::En));
        let numbers: Vec<&str> = s.iter().map(|s| s.number.as_str()).collect();
        assert_eq!(numbers, ["110", "112", "*8", "*761", "*760", "#21#", "*97", "*43"]);
        // every code without an argument has a phone book name, codes with one are left out
        assert!(FEATURE_CODES.iter().all(|f| f.arg.is_some() || !f.name.is_empty()));
        let vm = s.iter().find(|s| s.number == "*97").unwrap();
        assert_eq!((vm.uid.as_str(), vm.name.as_str(), vm.kind.as_str()), ("svc-s97", "Voicemail (*97)", "Feature code"));
        assert_eq!(s[1].name, "Emergency 112");
        assert_eq!(s.iter().find(|s| s.number == "#21#").unwrap().tel(), "tel:%2321%23");
        assert!(!vm.internal() && !s[0].internal());
        let de = fixed_services(&["112".into()], Tr(Lang::De));
        assert_eq!(de.iter().find(|s| s.number == "*761").unwrap().name, "Nicht stören an (*761)");
        assert_eq!(de[0].kind, "Notruf");
        // uids are unique
        let mut uids: Vec<&str> = s.iter().map(|s| s.uid.as_str()).collect();
        uids.sort();
        uids.dedup();
        assert_eq!(uids.len(), s.len());
    }
}
