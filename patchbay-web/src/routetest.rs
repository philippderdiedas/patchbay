// SPDX-License-Identifier: GPL-3.0-or-later
//! "Which way would this call take?" for admins: replays the decisions of the dialplan
//! (from-device, sub-internal, sub-outbound) for a dialled number and optionally a caller.

use sqlx::{FromRow, PgPool};

use crate::{AppError, admin::normalise, flow::tf, i18n::{Lang, Tr}, numbers};

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
    route_id: i32,
    name: String,
    priority: i16,
    match_prefix: String,
    action: String,
    route_class: String,
    strip_digits: i16,
    prepend: Option<String>,
}

/// One row of route_trunk_order(): the trunks a call tries, in order (same SQL as the dialplan).
#[derive(FromRow)]
struct TrunkTry {
    trunk_name: String,
    trunk_kind: String,
    clip_e164: String,
    preferred: bool,
    clip_own_only: bool,
}

/// A person's outbound number (users.outbound_did_id) and the trunk it belongs to.
#[derive(FromRow)]
pub struct OutboundNumber {
    pub e164: String,
    pub trunk: String,
    /// the trunk shows only its own numbers, but is switched off or in no dial rule that dials out
    /// showing the person's number (action route, clip_mode user): the number is never shown
    pub never_shown: bool,
}

/// SQL condition on trunk `t` (the trunk of an outbound number) for [`OutboundNumber::never_shown`];
/// also used by flow.rs.
pub const NEVER_SHOWN: &str = "(t.clip_own_only AND (NOT t.enabled OR t.deleted_at IS NOT NULL OR NOT EXISTS (
    SELECT 1 FROM route_trunks rt JOIN outbound_routes o ON o.id = rt.route_id
    WHERE rt.trunk_id = t.id AND o.action = 'route' AND o.clip_mode = 'user')))";

pub async fn outbound_number(db: &PgPool, user_id: i32) -> Result<Option<OutboundNumber>, AppError> {
    Ok(sqlx::query_as(&format!(
        "SELECT d.e164, t.name AS trunk, {NEVER_SHOWN} AS never_shown
         FROM users u JOIN dids d ON d.id = u.outbound_did_id JOIN trunks t ON t.id = d.trunk_id WHERE u.id = $1"
    ))
    .bind(user_id)
    .fetch_optional(db)
    .await
    .map_err(anyhow::Error::from)?)
}

/// Warning for a never shown outbound number ("{}" = number, trunk; route tester, person page, flow).
pub const NEVER_SHOWN_TEXT: &str =
    "Outbound number {} belongs to trunk {}, which shows only its own numbers, but that trunk is switched off or in no dial rule that dials out showing the person's number: the number is never shown.";

pub fn never_shown_text(o: &OutboundNumber, t: &Tr) -> String {
    tf(t, NEVER_SHOWN_TEXT, &[&o.e164, &o.trunk])
}

/// The caller ID one attempt sends, as sub-outbound sets it: a number of the own country (`cc`) as
/// 0…, any other number is not set and the caller's extension goes out unchanged.
fn clip_text(t: &TrunkTry, cc: &str) -> String {
    let own = |n: &str| (!cc.is_empty()).then(|| n.strip_prefix(&format!("+{cc}"))).flatten().map(|r| format!("0{r}"));
    match (t.trunk_kind.as_str(), t.clip_e164.as_str(), t.clip_own_only) {
        ("iax2", _, _) => "caller ID: the extension".into(),
        (_, "", true) => "caller ID: none set (the extension); the provider shows the trunk's own number".into(),
        (_, "", false) => "caller ID: none set, the extension goes out".into(),
        (_, n, _) => match own(n) {
            Some(national) => format!("caller ID {national} ({n})"),
            None => format!("caller ID: {n} is outside the own country and not set, the extension goes out"),
        },
    }
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
        "SELECT f.route_id, r.name, r.priority, r.match_prefix, f.action, f.route_class, f.strip_digits, f.prepend
         FROM route_for($1) f JOIN outbound_routes r ON r.id = f.route_id",
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
    // the caller (CALLER_INFO: a person with an active device) decides the trunk order and caller ID
    let caller = caller.trim();
    let info: Option<(i32, String, Vec<String>, String)> = if caller.is_empty() {
        None
    } else {
        sqlx::query_as(
            "SELECT u.id, u.display_name, e.classes, COALESCE(d.e164, '') FROM users u
             JOIN user_effective_classes e ON e.user_id = u.id LEFT JOIN dids d ON d.id = u.outbound_did_id
             WHERE u.extension::text = $1 AND u.deleted_at IS NULL
               AND EXISTS (SELECT 1 FROM active_devices a WHERE a.user_id = u.id)",
        )
        .bind(caller)
        .fetch_optional(db)
        .await
        .map_err(anyhow::Error::from)?
    };
    let caller_did = info.as_ref().map(|i| i.3.clone()).unwrap_or_default();
    // route_trunk_order(): the same order and caller IDs the dialplan gets (func_odbc ROUTE)
    let tries: Vec<TrunkTry> = sqlx::query_as(
        "SELECT o.trunk_name, o.trunk_kind, o.clip_e164, o.preferred, t.clip_own_only
         FROM route_trunk_order($1, $2) o JOIN trunks t ON t.id = o.trunk_id ORDER BY o.attempt",
    )
    .bind(r.route_id)
    .bind(&caller_did)
    .fetch_all(db)
    .await
    .map_err(anyhow::Error::from)?;
    let off: Vec<String> = sqlx::query_scalar(
        "SELECT t.name FROM route_trunks rt JOIN trunks t ON t.id = rt.trunk_id
         WHERE rt.route_id = $1 AND NOT (t.enabled AND t.deleted_at IS NULL) ORDER BY rt.position",
    )
    .bind(r.route_id)
    .fetch_all(db)
    .await
    .map_err(anyhow::Error::from)?;
    if !off.is_empty() {
        steps.push(step("Skipped", format!("switched off or deleted: {}", off.join(", "))));
    }
    let Some(first) = tries.first() else {
        return done(steps, false, "The dial rule has no trunk that is switched on.".into());
    };
    for (i, t) in tries.iter().enumerate() {
        let mut v = format!("{}, dials {out}, {}", t.trunk_name, clip_text(t, crate::admin::country_code()));
        if t.preferred {
            v += &format!(" – first because the caller's number {caller_did} belongs to it");
        }
        if i > 0 {
            // sub-outbound "final": channel limit (not for emergency calls), or CHANUNAVAIL/CONGESTION
            // before any ringing and not for a reason of the number
            v += if r.route_class == "emergency" {
                " – only if the trunk before could not take the call (unreachable or congested, before any ringing)"
            } else {
                " – only if the trunk before could not take the call (unreachable, congested or at its call limit, before any ringing)"
            };
        }
        steps.push(step(if i == 0 { "Trunk" } else { "Fallback" }, v));
    }
    if r.route_class == "emergency" {
        steps.push(step("Emergency", "Emergency calls keep the rule's order; if no trunk takes the call, the emergency fallback trunk (PBX_EMERGENCY_TRUNK)."));
    }
    let via = first.trunk_name.clone();
    steps.push(step("Permission class", r.route_class.clone()));
    if caller.is_empty() {
        return done(steps, true, format!("Goes out over {via} as {out} (permission not checked: no caller given)."));
    }
    let Some((uid, name, classes, _)) = info else {
        return done(steps, false, format!("Caller {caller} is not a person with an active device and phone access."));
    };
    steps.push(step("Caller", format!("{caller} · {name}: {}", classes.join(", "))));
    if let Some(o) = outbound_number(db, uid).await?.filter(|o| o.never_shown) {
        steps.push(step("Warning", never_shown_text(&o, &Tr(Lang::En))));
    }
    if r.route_class == "emergency" || classes.contains(&r.route_class) {
        done(steps, true, format!("{name} may make this call: out over {via} as {out}."))
    } else {
        done(steps, false, format!("{name} lacks the permission class “{}”.", r.route_class))
    }
}

#[cfg(test)]
mod tests {
    use super::{Pattern, TrunkTry, clip_text, pattern};

    #[test]
    fn caller_id_per_trunk() {
        let t = |kind: &str, clip: &str, own: bool| {
            let tr = TrunkTry { trunk_name: "x".into(), trunk_kind: kind.into(), clip_e164: clip.into(), preferred: false, clip_own_only: own };
            clip_text(&tr, "49")
        };
        assert_eq!(t("sip_register", "+494012345054", true), "caller ID 04012345054 (+494012345054)");
        assert_eq!(t("sip_register", "", true), "caller ID: none set (the extension); the provider shows the trunk's own number");
        assert_eq!(t("sip_ip", "", false), "caller ID: none set, the extension goes out");
        assert_eq!(t("sip_ip", "+43151234", false), "caller ID: +43151234 is outside the own country and not set, the extension goes out");
        assert_eq!(t("iax2", "+494012345054", false), "caller ID: the extension");
    }

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

    /// The example of docs/configuration.md against a throwaway, migrated database (inserts its own
    /// rows, ids 901…, its dial rule at priority 1): PBX_TEST_DATABASE_URL=postgres://… cargo test routetest -- --ignored
    #[tokio::test]
    #[ignore]
    async fn trunk_order_by_caller_number() {
        let url = std::env::var("PBX_TEST_DATABASE_URL").expect("PBX_TEST_DATABASE_URL=postgres://…");
        let db = sqlx::PgPool::connect(&url).await.unwrap();
        crate::admin::init_dialing("49", &["112".to_string()]);
        sqlx::raw_sql(
            "INSERT INTO trunks (id, name, kind, asterisk_ref, managed, clip_own_only) VALUES
                 (901, 'T053', 'sip_register', 'sip-901', true, true), (902, 'T054', 'sip_register', 'sip-902', true, true),
                 (903, 'T055', 'sip_register', 'sip-903', true, true);
             INSERT INTO dids (id, trunk_id, e164) VALUES (901, 901, '+499912345053'), (902, 902, '+499912345054'), (903, 903, '+499912345055');
             INSERT INTO outbound_routes (id, priority, name, match_prefix, min_len, action, clip_mode, route_class) VALUES
                 (901, 1, 'Extern', '0', 2, 'route', 'user', 'national');
             INSERT INTO route_trunks (route_id, position, trunk_id) VALUES (901, 1, 903), (901, 2, 902);
             INSERT INTO users (id, extension, display_name, outbound_did_id) VALUES (901, 1901, 'Tim', 902), (902, 1902, 'Ann', NULL),
                 (903, 1903, 'Eve', 901);
             INSERT INTO devices (user_id, label, type, sip_username, ha1)
                 SELECT id, 'phone', 'generic', 'routetest' || id, md5('x') FROM users WHERE id IN (901, 902, 903);",
        )
        .execute(&db)
        .await
        .unwrap();
        let show = |r: &super::TestResult| r.steps.iter().map(|s| format!("{}: {}\n", s.label, s.value)).collect::<String>();
        let tim = super::evaluate(&db, "0451 123", "1901").await.unwrap();
        let t = show(&tim);
        assert!(tim.ok && tim.verdict.contains("out over T054"), "{t}");
        assert!(t.contains("Trunk: T054, dials 0451123, caller ID 09912345054 (+499912345054) – first because"), "{t}");
        assert!(t.contains("Fallback: T055, dials 0451123, caller ID: none set (the extension); the provider shows the trunk's own number"), "{t}");
        let ann = super::evaluate(&db, "0451 123", "1902").await.unwrap();
        let a = show(&ann);
        assert!(ann.verdict.contains("out over T055"), "{a}");
        assert!(a.contains("Fallback: T054") && !a.contains("first because"), "{a}");
        // Eve's number belongs to T053, which shows only its own numbers and is in no dial rule
        let eve = super::evaluate(&db, "0451 123", "1903").await.unwrap();
        let e = show(&eve);
        assert!(e.contains("Warning: Outbound number +499912345053 belongs to trunk T053"), "{e}");
        sqlx::query("UPDATE trunks SET enabled = false WHERE id = 902").execute(&db).await.unwrap();
        let t = show(&super::evaluate(&db, "0451 123", "1901").await.unwrap());
        assert!(t.contains("Skipped: switched off or deleted: T054") && t.contains("Trunk: T055"), "{t}");
        let may: bool = sqlx::query_scalar("SELECT may_dial(901, '0451123')").fetch_one(&db).await.unwrap();
        assert!(may);
    }
}
