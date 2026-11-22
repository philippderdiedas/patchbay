// SPDX-License-Identifier: GPL-3.0-or-later
//! Custom apps: admin-defined dialplan blocks written as MiniJinja templates.
//! Each enabled app becomes a context [app-custom-<id>] in /generated/extensions_custom.conf,
//! reached through its internal number (number plan, company range). A background job re-renders all apps every
//! minute so that templates follow data changes (users, groups, IVRs, sounds).

use std::{collections::BTreeMap, time::Duration};

use anyhow::Result;
use askama::Template;
use axum::{
    Form,
    extract::{Path, State},
    response::{IntoResponse, Redirect, Response},
};
use minijinja::{Environment, UndefinedBehavior, context};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::{AppError, AppState, ami, auth::Admin, db, ui::{Ctx, render as render_page}};

const OUT: &str = "/generated/extensions_custom.conf";


// --- Template data ----------------------------------------------------------------------

#[derive(Serialize, FromRow)]
struct UserData {
    extension: i16,
    name: String,
    email: Option<String>,
}

#[derive(Serialize, FromRow)]
struct GroupData {
    number: i16,
    name: String,
}

#[derive(Serialize, FromRow)]
struct IvrData {
    id: i32,
    name: String,
    number: Option<i16>,
}

#[derive(Serialize, FromRow)]
struct SoundData {
    id: i32,
    name: String,
    /// path for Playback(), e.g. "pbx/7"
    file: String,
}

/// Data available in templates. Maps are keyed by extension/number/name, e.g.
/// {{ users["1021"].extension }}, {{ groups["5120"].name }}, {{ ivrs["Main menu EN"].number }},
/// {{ sounds["de_main"].file }}, {{ site.realm }}.
async fn template_data(app: &AppState) -> Result<minijinja::Value> {
    let users: Vec<UserData> =
        sqlx::query_as("SELECT extension, display_name AS name, email FROM users WHERE deleted_at IS NULL").fetch_all(&app.db).await?;
    let groups: Vec<GroupData> = sqlx::query_as("SELECT number, name FROM groups WHERE deleted_at IS NULL").fetch_all(&app.db).await?;
    let ivrs: Vec<IvrData> = sqlx::query_as("SELECT id, name, number FROM ivrs WHERE deleted_at IS NULL").fetch_all(&app.db).await?;
    let sounds: Vec<SoundData> =
        sqlx::query_as("SELECT id, name, 'pbx/' || id AS file FROM sounds WHERE deleted_at IS NULL").fetch_all(&app.db).await?;
    // keys keep the original names (lookups in templates); values are made dialplan-safe
    let users: BTreeMap<String, UserData> = users
        .into_iter()
        .map(|u| (u.extension.to_string(), UserData { name: dp_text(&u.name), email: u.email.as_deref().map(dp_text), ..u }))
        .collect();
    let groups: BTreeMap<String, GroupData> = groups.into_iter().map(|g| (g.number.to_string(), GroupData { name: dp_text(&g.name), ..g })).collect();
    let ivrs: BTreeMap<String, IvrData> = ivrs.into_iter().map(|i| (i.name.clone(), IvrData { name: dp_text(&i.name), ..i })).collect();
    let sounds: BTreeMap<String, SoundData> = sounds.into_iter().map(|s| (s.name.clone(), SoundData { name: dp_text(&s.name), ..s })).collect();
    Ok(context! {
        users => users,
        groups => groups,
        ivrs => ivrs,
        sounds => sounds,
        site => context! { realm => dp_text(&app.cfg.sip_realm), title => dp_text(&app.cfg.site_title) },
    })
}

/// Data text (names, e-mail addresses) as it may appear inside dialplan arguments: letters, digits,
/// space and . - _ @ + only. Everything else (, ; ( ) $ [ ] { } " ' \ ? : | & newlines …) could
/// end an argument, start a comment or inject applications/functions, and becomes a space.
fn dp_text(s: &str) -> String {
    let t: String = s.chars().map(|c| if c.is_alphanumeric() || " .-_@+".contains(c) { c } else { ' ' }).collect();
    t.trim().to_string()
}

fn render(template: &str, data: &minijinja::Value) -> Result<String, String> {
    let mut env = Environment::new();
    // Unknown names are errors, so typos do not silently produce empty dialplan arguments
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.add_template("app", template).map_err(|e| e.to_string())?;
    env.get_template("app").and_then(|t| t.render(data)).map_err(|e| e.to_string())
}

/// Guard rails (PBX_CUSTOM_DIALPLAN_GUARD=strict): an allowlist, not a blocklist.
/// Every line must be `exten =>`/`same =>` with an allowed application; dialplan functions must be
/// on the function allowlist; jumps may only go to the app's own context (labels, "exten,prio" or
/// "app-custom-<id>,exten,prio"; other custom apps pass the same guard) or to the PBX entry points
/// that apply the caller's own rights (from-device, sub-outbound, sub-user, sub-group, sub-ivr);
/// Set()/Read() may only write plain variables (see `writable`); dynamic variable names and EVAL
/// are rejected.
fn guard(rendered: &str) -> Vec<String> {
    const APPS: &[&str] = &[
        "answer", "hangup", "wait", "waitexten", "waitdigit", "playback", "background", "read", "controlplayback",
        "saydigits", "saynumber", "sayalpha", "sayunixtime", "set", "noop", "verbose", "goto", "gotoif", "gotoiftime",
        "gosub", "gosubif", "return", "echo", "musiconhold", "startmusiconhold", "stopmusiconhold", "ringing",
        "progress", "busy", "congestion", "playtones", "stopplaytones", "confbridge",
    ];
    const FUNCS: &[&str] = &[
        "CALLERID", "IF", "IFTIME", "LEN", "FILTER", "CUT", "RAND", "STRFTIME", "LANGUAGE", "ISNULL", "EXISTS",
        "FIELDQTY", "FIELDNUM", "TOUPPER", "TOLOWER", "REPLACE", "REGEX", "CONFBRIDGE", "CONFBRIDGE_INFO",
    ];
    const TARGETS: &[&str] = &["from-device", "sub-outbound", "sub-user", "sub-group", "sub-ivr"];
    let mut errors = vec![];
    for (n, line) in rendered.lines().enumerate() {
        let n = n + 1;
        let l = line.trim();
        if l.is_empty() || l.starts_with(';') {
            continue;
        }
        let Some((head, body)) = l.split_once("=>") else {
            errors.push(format!("line {n}: only `exten =>` and `same =>` lines are allowed"));
            continue;
        };
        let head = head.trim().to_lowercase();
        // exten => <pattern>,<prio>,<App>(…) | same => <prio>,<App>(…)
        let parts = if head == "exten" { 3 } else if head == "same" { 2 } else {
            errors.push(format!("line {n}: only `exten =>` and `same =>` lines are allowed"));
            continue;
        };
        let Some(app_part) = body.trim().splitn(parts, ',').nth(parts - 1) else {
            errors.push(format!("line {n}: application missing"));
            continue;
        };
        let (app_name, args) = match app_part.split_once('(') {
            Some((a, rest)) => (a.trim().to_lowercase(), rest.trim_end().trim_end_matches(')').to_string()),
            None => (app_part.trim().to_lowercase(), String::new()),
        };
        if !APPS.contains(&app_name.as_str()) {
            errors.push(format!("line {n}: application {app_name}() is not allowed"));
        }
        if l.contains("${${") || l.to_uppercase().contains("EVAL(") {
            errors.push(format!("line {n}: dynamic variable names and EVAL are not allowed"));
        }
        // functions inside ${…} / $[…]: NAME( directly after ${ or another separator
        let up = l.to_string();
        let mut i = 0;
        while let Some(pos) = up[i..].find("${") {
            let s = &up[i + pos + 2..];
            let name: String = s.chars().take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_').collect();
            if s[name.len()..].starts_with('(') && !FUNCS.contains(&name.as_str()) {
                errors.push(format!("line {n}: function {name}() is not allowed"));
            }
            i += pos + 2;
        }
        // written variables: Set(VAR=…) and Read(VAR,…)
        let written = match app_name.as_str() {
            "set" => Some(args.split_once('=').map(|(v, _)| v).unwrap_or(&args)),
            "read" => Some(args.split(',').next().unwrap_or("")),
            _ => None,
        };
        if let Some(var) = written {
            if !writable(var.trim()) {
                errors.push(format!("line {n}: {} cannot be set here (plain variable names only, not PBX_*)", var.trim()));
            }
        }
        if matches!(app_name.as_str(), "goto" | "gotoif" | "gotoiftime" | "gosub" | "gosubif") {
            // targets: "label", "exten,prio", "context,exten,prio" (GotoIf: "cond?a:b"); a context
            // must be a PBX entry point or a custom app context (all of them pass this guard)
            let targets = args.split_once('?').map(|(_, t)| t.to_string()).unwrap_or(args.clone());
            for target in targets.split(':') {
                let fields: Vec<&str> = target.split(',').collect();
                let ctx = fields[0].trim();
                let custom = ctx.strip_prefix("app-custom-").is_some_and(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()));
                if fields.len() >= 3 && !TARGETS.contains(&ctx) && !custom {
                    errors.push(format!("line {n}: jumps only within the app, to app-custom-<id> or to {}", TARGETS.join(", ")));
                }
            }
        }
    }
    errors
}

/// May a custom app write this variable? Plain channel variables (optionally inherited with _ or
/// __), not the PBX's own PBX_* variables, and of the dialplan functions only CALLERID(name) and
/// CONFBRIDGE(…). Everything else (GLOBAL(), DB(), FILE(), SHARED(), CHANNEL(), …) is refused:
/// e.g. Set(GLOBAL(PBX_ALLOW_EXTERNAL)=…) would switch off the permission check for every caller.
fn writable(var: &str) -> bool {
    let up = var.to_uppercase();
    if up == "CALLERID(NAME)" || (up.starts_with("CONFBRIDGE(") && up.ends_with(')') && !up.contains("${")) {
        return true;
    }
    let name = up.strip_prefix("__").or_else(|| up.strip_prefix('_')).unwrap_or(&up);
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with("PBX_")
}

/// Renders all enabled apps into one config file. Returns (file content, errors per app).
async fn build_all(app: &AppState) -> Result<(String, Vec<(i32, String)>)> {
    let data = template_data(app).await?;
    let apps: Vec<(i32, String, String)> =
        sqlx::query_as("SELECT id, name, template FROM custom_apps WHERE enabled AND deleted_at IS NULL ORDER BY id")
            .fetch_all(&app.db)
            .await?;
    let mut out = String::from("; Generated by patchbay-web from custom app templates. Do not edit.\n");
    let mut errors = vec![];
    for (id, name, template) in apps {
        match render(&template, &data) {
            Ok(r) => {
                let g = if app.cfg.custom_dialplan_guard { guard(&r) } else { vec![] };
                if g.is_empty() {
                    out += &format!("\n; custom app {id}: {}\n[app-custom-{id}]\n{}\n", name.replace('\n', " "), r.trim_end());
                } else {
                    errors.push((id, g.join("; ")));
                }
            }
            Err(e) => errors.push((id, e)),
        }
    }
    Ok((out, errors))
}

/// Writes the file and reloads the dialplan if it changed. On a reload problem the previous
/// file is restored, so a broken app never takes the running dialplan down.
async fn apply(app: &AppState) -> Result<bool> {
    let (content, _) = build_all(app).await?;
    let current = tokio::fs::read_to_string(OUT).await.unwrap_or_default();
    if current == content {
        return Ok(false);
    }
    tokio::fs::write(OUT, &content).await?;
    let out = ami::command("dialplan reload").await?;
    if !out.to_lowercase().contains("reloaded") {
        tokio::fs::write(OUT, &current).await?;
        ami::command("dialplan reload").await?;
        anyhow::bail!("dialplan reload failed, previous custom apps restored: {out}");
    }
    Ok(true)
}

pub fn spawn(app: AppState) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(20)).await;
        loop {
            match apply(&app).await {
                Ok(true) => tracing::info!("custom apps re-rendered and dialplan reloaded"),
                Ok(false) => {}
                Err(e) => tracing::warn!("custom apps: {e:#}"),
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}

// --- Admin pages ---------------------------------------------------------------------------

#[derive(FromRow)]
pub struct AppRow {
    id: i32,
    number: i16,
    name: String,
    template: String,
    enabled: bool,
}

#[derive(Template)]
#[template(path = "admin_apps.html")]
struct AppsPage {
    c: Ctx,
    apps: Vec<AppRow>,
    company: Vec<crate::apps::AppRow>,
    kinds: &'static [&'static str],
    guard: bool,
}

#[derive(Template)]
#[template(path = "admin_app.html")]
struct AppPage {
    c: Ctx,
    a: AppRow,
    preview: Option<String>,
    errors: Vec<String>,
    guard: bool,
    is_new: bool,
}

pub async fn apps(State(app): State<AppState>, Admin(u): Admin) -> Result<Response, AppError> {
    let apps = sqlx::query_as("SELECT id, number, name, template, enabled FROM custom_apps WHERE deleted_at IS NULL ORDER BY number")
        .fetch_all(&app.db)
        .await
        .map_err(anyhow::Error::from)?;
    let company = crate::apps::company_apps(&app.db).await?;
    render_page(AppsPage { c: Ctx::new(&app, &u).section("apps"), apps, company, kinds: crate::apps::KINDS, guard: app.cfg.custom_dialplan_guard })
}

const EXAMPLE: &str = "exten => s,1,Answer()\n same => n,Playback({{ sounds[\"de_main\"].file }})\n same => n,Goto(from-device,{{ groups[\"5120\"].number }},1)\n";

pub async fn app_new(State(app): State<AppState>, Admin(u): Admin) -> Result<Response, AppError> {
    let a = AppRow { id: 0, number: 0, name: String::new(), template: EXAMPLE.into(), enabled: true };
    render_page(AppPage { c: Ctx::new(&app, &u).section("apps"), a, preview: None, errors: vec![], guard: app.cfg.custom_dialplan_guard, is_new: true })
}

pub async fn app_edit(State(app): State<AppState>, Admin(u): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    let a: AppRow = sqlx::query_as("SELECT id, number, name, template, enabled FROM custom_apps WHERE id = $1 AND deleted_at IS NULL")
        .bind(id)
        .fetch_optional(&app.db)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or(AppError::NotFound)?;
    render_page(AppPage { c: Ctx::new(&app, &u).section("apps"), a, preview: None, errors: vec![], guard: app.cfg.custom_dialplan_guard, is_new: false })
}

#[derive(Deserialize)]
pub struct AppForm {
    number: i16,
    name: String,
    template: String,
    #[serde(default)]
    enabled: Option<String>,
    /// "preview" or "save"
    action: String,
}


/// Unified diff-like summary of a template change for the audit log.
fn diff(old: &str, new: &str) -> String {
    let (o, n): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let mut d = String::new();
    for l in &o {
        if !n.contains(l) {
            d += &format!("- {l}\n");
        }
    }
    for l in &n {
        if !o.contains(l) {
            d += &format!("+ {l}\n");
        }
    }
    d.chars().take(4000).collect()
}

pub async fn app_save(State(app): State<AppState>, Admin(u): Admin, Path(id): Path<String>, Form(f): Form<AppForm>) -> Result<Response, AppError> {
    let is_new = id == "new";
    let id: i32 = if is_new { 0 } else { id.parse().map_err(|_| AppError::NotFound)? };
    let a = AppRow { id, number: f.number, name: f.name.trim().to_string(), template: f.template.replace("\r\n", "\n"), enabled: f.enabled.is_some() };
    let mut errors = vec![];
    if let Err(AppError::BadRequest(m)) =
        crate::numbers::check(&app.db, &app.cfg.numbers, "custom_app", &a.number.to_string(), (!is_new).then_some(id)).await
    {
        errors.push(m);
    }
    if a.name.is_empty() {
        errors.push("Name missing.".to_string());
    }
    let data = template_data(&app).await?;
    let preview = match render(&a.template, &data) {
        Ok(r) => {
            if app.cfg.custom_dialplan_guard {
                errors.extend(guard(&r));
            }
            Some(format!("[app-custom-{}]\n{}", if is_new { "<new>".to_string() } else { id.to_string() }, r))
        }
        Err(e) => {
            errors.push(format!("Template error: {e}"));
            None
        }
    };
    if f.action != "save" || !errors.is_empty() {
        return render_page(AppPage { c: Ctx::new(&app, &u).section("apps"), a, preview, errors, guard: app.cfg.custom_dialplan_guard, is_new });
    }

    let old: Option<String> = sqlx::query_scalar("SELECT template FROM custom_apps WHERE id = $1").bind(id).fetch_optional(&app.db).await.map_err(anyhow::Error::from)?;
    let rendered = preview.clone().unwrap_or_default();
    let saved_id: i32 = if is_new {
        sqlx::query_scalar("INSERT INTO custom_apps (number, name, template, rendered, enabled) VALUES ($1, $2, $3, $4, $5) RETURNING id")
            .bind(a.number)
            .bind(&a.name)
            .bind(&a.template)
            .bind(&rendered)
            .bind(a.enabled)
            .fetch_one(&app.db)
            .await
    } else {
        sqlx::query_scalar(
            "UPDATE custom_apps SET number = $2, name = $3, template = $4, rendered = $5, enabled = $6, updated_at = now()
             WHERE id = $1 AND deleted_at IS NULL RETURNING id",
        )
        .bind(id)
        .bind(a.number)
        .bind(&a.name)
        .bind(&a.template)
        .bind(&rendered)
        .bind(a.enabled)
        .fetch_one(&app.db)
        .await
    }
    .map_err(|e| AppError::BadRequest(format!("Saving failed: {e}")))?;
    db::audit(
        &app.db,
        &u.session.sub,
        if is_new { "custom_app.add" } else { "custom_app.edit" },
        &format!("custom-app:{saved_id}"),
        serde_json::json!({"number": a.number, "enabled": a.enabled, "diff": diff(old.as_deref().unwrap_or(""), &a.template)}),
    )
    .await?;

    // Activate now and verify that Asterisk loaded the context
    if let Err(e) = apply(&app).await {
        return Err(AppError::BadRequest(format!("Saved, but activation failed: {e:#}")));
    }
    if a.enabled {
        let out = ami::command(&format!("dialplan show app-custom-{saved_id}")).await.unwrap_or_default();
        if !out.contains(&format!("app-custom-{saved_id}")) || out.contains("There is no existence") {
            return Err(AppError::BadRequest(format!(
                "Saved, but Asterisk did not load the context app-custom-{saved_id}. Check the dialplan syntax (e.g. \"exten => s,1,...\")."
            )));
        }
    }
    Ok(Redirect::to(&format!("/admin/apps/{saved_id}")).into_response())
}

pub async fn app_delete(State(app): State<AppState>, Admin(u): Admin, Path(id): Path<i32>) -> Result<Response, AppError> {
    sqlx::query("UPDATE custom_apps SET deleted_at = now() WHERE id = $1").bind(id).execute(&app.db).await.map_err(anyhow::Error::from)?;
    db::audit(&app.db, &u.session.sub, "custom_app.delete", &format!("custom-app:{id}"), serde_json::json!({})).await?;
    let _ = apply(&app).await;
    Ok(Redirect::to("/admin/apps").into_response())
}

#[cfg(test)]
mod tests {
    use super::{dp_text, guard};

    #[test]
    fn guard_blocks_shell_and_context_breakout() {
        assert!(!guard("exten => s,1,System(rm -rf /)").is_empty());
        assert!(!guard("[from-device]\nexten => s,1,NoOp()").is_empty());
        assert!(!guard("exten => s,1,Set(X=${SHELL(id)})").is_empty());
        assert!(guard("exten => s,1,Answer()\n same => n,Playback(pbx/7)").is_empty());
        assert!(!guard("exten => s,1,Set(PBX_ALLOW_EXTERNAL=1)").is_empty());
        assert!(!guard("exten => s,1,Set(F=SHE)\n same => n,NoOp(${${F}LL(id)})").is_empty());
        assert!(!guard("exten => s,1,Goto(test-call,0049,1)").is_empty());
        assert!(!guard("exten => s,1,Dial(PJSIP/0049@sip-3)").is_empty());
        assert!(!guard("exten => s,1,MixMonitor(x.wav,,/bin/sh)").is_empty());
        assert!(guard("exten => s,1,Answer()\n same => n,Read(D,beep,1)\n same => n,GotoIf($[\"${D}\" = \"1\"]?from-device,20,1:bye)\n same => n(bye),Hangup()").is_empty());
        assert!(guard("exten => s,1,Gosub(sub-outbound,s,1(${CALLERID(num)}))").is_empty());
    }

    #[test]
    fn guard_allows_only_plain_variables() {
        for bad in [
            "Set(GLOBAL(PBX_ALLOW_EXTERNAL)=0049)",
            "Set(GLOBAL(X)=1)",
            "Set(DB(a/b)=1)",
            "Set(FILE(/etc/asterisk/x.conf)=1)",
            "Set(SHARED(X)=1)",
            "Set(CHANNEL(language)=en)",
            "Set(__PBX_ALLOW_EXTERNAL=0049)",
            "Set(pbx_kinds=person)",
            "Set(CALLERID(num)=0800)",
            "Read(PBX_ALLOW_EXTERNAL,beep,20)",
            "Read(GLOBAL(X),beep,1)",
        ] {
            assert!(!guard(&format!("exten => s,1,{bad}")).is_empty(), "{bad}");
        }
        for ok in ["Set(X=1)", "Set(__LANG=de)", "Set(CALLERID(name)=Support)", "Set(CONFBRIDGE(user,admin)=yes)", "Read(D,beep,1)"] {
            assert!(guard(&format!("exten => s,1,{ok}")).is_empty(), "{ok}");
        }
    }

    #[test]
    fn guard_allows_own_context_jumps() {
        assert!(guard("exten => s,1,Goto(app-custom-12,s,1)").is_empty());
        assert!(guard("exten => s,1,GotoIf($[1]?app-custom-3,s,1:from-device,1020,1)").is_empty());
        assert!(guard("exten => s,1,Goto(s,1)").is_empty());
        assert!(!guard("exten => s,1,Goto(app-custom-x,s,1)").is_empty());
        assert!(!guard("exten => s,1,Goto(sub-mailbox,s,1)").is_empty());
    }

    #[test]
    fn data_text_is_dialplan_safe() {
        assert_eq!(dp_text("Anna Müller"), "Anna Müller");
        assert_eq!(dp_text("a.b-c@example.org"), "a.b-c@example.org");
        assert!(guard(&format!("exten => s,1,NoOp({})", dp_text("x)\n same => n,System(id)"))).is_empty());
        assert!(!dp_text("a;b,c${X}[d]").contains(|c: char| ";,${}[]()".contains(c)));
    }
}

/// POST /admin/apps/new (see trunks::trunk_create)
pub async fn app_create(state: State<AppState>, admin: Admin, form: Form<AppForm>) -> Result<Response, AppError> {
    app_save(state, admin, Path("new".to_string()), form).await
}
