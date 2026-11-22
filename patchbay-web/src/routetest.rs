// SPDX-License-Identifier: GPL-3.0-or-later
//! "Which way would this call take?" for admins: replays the decisions of the dialplan
//! (from-device, sub-internal, sub-outbound) for a dialled number and optionally a caller.

use sqlx::{FromRow, PgPool};

use crate::{AppError, admin::normalise, numbers};

pub struct Step {
    pub label: &'static str,
    pub value: String,
}

pub struct TestResult {
    pub number: String,
    pub steps: Vec<Step>,
    /// final verdict
    pub ok: bool,
    pub verdict: String,
}

#[derive(FromRow)]
struct RouteRow {
    name: String,
    priority: i16,
    match_prefix: String,
    action: String,
    route_class: String,
    strip_digits: i16,
    prepend: Option<String>,
    trunk: Option<String>,
    trunk_ok: bool,
}

fn step(label: &'static str, value: impl Into<String>) -> Step {
    Step { label, value: value.into() }
}

/// Which pattern of [from-device] a number hits (same order of precedence as Asterisk).
enum Pattern {
    Internal,
    /// `_[1-9]XXXX.`: other site's prefix + number (the dial rules decide)
    Site,
    Outbound,
    Feature,
    None,
}

fn pattern(n: &str) -> Pattern {
    let digits = n.chars().all(|c| c.is_ascii_digit());
    let first = n.chars().next().unwrap_or('0');
    if n.starts_with('*') || n.starts_with('#') {
        Pattern::Feature
    } else if n.starts_with('+') && n.len() > 1 {
        Pattern::Outbound
    } else if !digits {
        Pattern::None
    } else if (first != '0' && (2..=3).contains(&n.len())) || (n.len() == 6 && n.starts_with("116")) {
        // short numbers (emergency, service) and 116xxx go to the dial rules
        Pattern::Outbound
    } else if first == '0' && n.len() >= 2 {
        Pattern::Outbound
    } else if first != '0' && n.len() == 4 {
        Pattern::Internal
    } else if first != '0' && n.len() >= 6 {
        Pattern::Site
    } else {
        Pattern::None
    }
}

/// Is the target of an internal number reachable? Same conditions as the dialplan lookups.
async fn internal_state(db: &PgPool, kind: &str, n: i16) -> Result<Option<&'static str>, AppError> {
    let q = match kind {
        "person" => "SELECT NOT (enabled AND phone_allowed) FROM users WHERE extension = $1 AND deleted_at IS NULL",
        "device" => "SELECT NOT EXISTS (SELECT 1 FROM active_devices WHERE number = $1)",
        "app" | "company_app" => "SELECT NOT EXISTS (SELECT 1 FROM app_dialplan WHERE number = $1)",
        "custom_app" => "SELECT NOT EXISTS (SELECT 1 FROM custom_apps WHERE number = $1 AND enabled AND deleted_at IS NULL)",
        _ => return Ok(None),
    };
    let blocked: Option<bool> = sqlx::query_scalar(q).bind(n).fetch_optional(db).await.map_err(anyhow::Error::from)?;
    Ok(match (kind, blocked.unwrap_or(true)) {
        (_, false) => None,
        ("person", true) => Some("The person is locked or has no phone access; the call does not ring."),
        ("device", true) => Some("The device is locked or its owner has no phone access (caller hears “invalid”)."),
        (_, true) => Some("The app is switched off (caller hears “invalid”)."),
    })
}

pub async fn evaluate(db: &PgPool, raw: &str, caller: &str) -> Result<TestResult, AppError> {
    let dialled: String = raw.chars().filter(|c| c.is_ascii_digit() || "+*#".contains(*c)).collect();
    let mut steps = vec![];
    let done = |steps: Vec<Step>, ok: bool, verdict: String| Ok(TestResult { number: dialled.clone(), steps, ok, verdict });
    if dialled.is_empty() {
        return done(steps, false, "Enter a number.".into());
    }
    let current = dialled.clone();
    let pat = pattern(&current);
    match pat {
        Pattern::Feature => return done(steps, true, "Feature code (see the help page).".into()),
        Pattern::None => return done(steps, false, "No dial pattern matches (caller hears “invalid”).".into()),
        Pattern::Internal => {
            let num: i16 = current.parse().unwrap_or(0);
            return match numbers::lookup(db, num).await? {
                Some((kind, name)) => {
                    steps.push(step("Internal number", format!("{num}: {} · {name}", numbers::kind_label(&kind))));
                    match internal_state(db, &kind, num).await? {
                        Some(problem) => done(steps, false, problem.into()),
                        None => done(steps, true, format!("Internal call to {name}.")),
                    }
                }
                None => done(steps, false, format!("Internal number {num} is not assigned (caller hears “invalid”).")),
            };
        }
        Pattern::Site | Pattern::Outbound => {}
    }
    let dialled_out = current.clone();
    let n = normalise(&dialled_out);
    if n != dialled_out {
        steps.push(step("Normalised", n.clone()));
    }
    let route: Option<RouteRow> = sqlx::query_as(
        // route_for(): the same rule the dialplan (func_odbc ROUTE) picks
        "SELECT r.name, r.priority, r.match_prefix, f.action, f.route_class, f.strip_digits, f.prepend,
                t.name AS trunk, f.trunk <> '' AS trunk_ok
         FROM route_for($1) f JOIN outbound_routes r ON r.id = f.route_id LEFT JOIN trunks t ON t.id = r.trunk_id",
    )
    .bind(&n)
    .fetch_optional(db)
    .await
    .map_err(anyhow::Error::from)?;
    let Some(r) = route else {
        return done(steps, false, "No dial rule matches (caller hears “invalid”).".into());
    };
    steps.push(step("Dial rule", format!("{} (priority {}, prefix “{}”)", r.name, r.priority, r.match_prefix)));
    if r.action == "deny" {
        return done(steps, false, format!("Blocked by the dial rule “{}”.", r.name));
    }
    if r.action != "route" {
        return done(steps, false, format!("Dial rule action “{}” does not dial out.", r.action));
    }
    let out = format!("{}{}", r.prepend.clone().unwrap_or_default(), n.get(r.strip_digits.max(0) as usize..).unwrap_or(""));
    match (&r.trunk, r.trunk_ok) {
        (Some(t), true) => steps.push(step("Trunk", format!("{t}, dials {out}"))),
        (Some(t), false) => return done(steps, false, format!("Trunk {t} is switched off or deleted.")),
        (None, _) => return done(steps, false, "The dial rule has no trunk.".into()),
    }
    steps.push(step("Permission class", r.route_class.clone()));
    let caller = caller.trim();
    if caller.is_empty() {
        return done(steps, true, format!("Goes out over {} as {out} (permission not checked: no caller given).", r.trunk.unwrap_or_default()));
    }
    let info: Option<(String, Vec<String>)> = sqlx::query_as(
        "SELECT u.display_name, e.classes FROM users u JOIN user_effective_classes e ON e.user_id = u.id
         WHERE u.extension::text = $1 AND u.deleted_at IS NULL
           AND EXISTS (SELECT 1 FROM active_devices a WHERE a.user_id = u.id)",
    )
    .bind(caller)
    .fetch_optional(db)
    .await
    .map_err(anyhow::Error::from)?;
    let Some((name, classes)) = info else {
        return done(steps, false, format!("Caller {caller} is not a person with an active device and phone access."));
    };
    steps.push(step("Caller", format!("{caller} · {name}: {}", classes.join(", "))));
    if r.route_class == "emergency" || classes.contains(&r.route_class) {
        done(steps, true, format!("{name} may make this call: out over {} as {out}.", r.trunk.unwrap_or_default()))
    } else {
        done(steps, false, format!("{name} lacks the permission class “{}”.", r.route_class))
    }
}

#[cfg(test)]
mod tests {
    use super::{Pattern, pattern};

    #[test]
    fn patterns_like_from_device() {
        let p = |n: &str| match pattern(n) {
            Pattern::Internal => "internal",
            Pattern::Site => "site",
            Pattern::Outbound => "out",
            Pattern::Feature => "feature",
            Pattern::None => "none",
        };
        assert_eq!(p("1020"), "internal");
        assert_eq!(p("12345"), "none"); // 5 digits: no pattern
        assert_eq!(p("781020"), "site");
        assert_eq!(p("110"), "out");
        assert_eq!(p("116117"), "out");
        assert_eq!(p("0451123"), "out");
        assert_eq!(p("0"), "none");
        assert_eq!(p("+49451"), "out");
        assert_eq!(p("*761"), "feature");
        assert_eq!(p("12"), "out"); // 2–3 digits: short numbers (e.g. 15, 112, 911)
        assert_eq!(p("911"), "out");
        assert_eq!(p("1"), "none");
    }
}
