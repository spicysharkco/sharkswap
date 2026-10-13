//! The read-only account views: `best` (rank the saved accounts), `list`
//! usage cells and the status-bar `bar` line. None of them asks a provider.
//!
//! `best --raw` prints one tab-separated line per alias and nothing else, which
//! is the parsing contract for scripts:
//!
//! ```text
//! <alias>  <week% used|->  <week resets at|->  <reading age, days|->  <state>  <5h% used|->
//! ```
//!
//! State is
//!   fresh      a current reading, neither window spent
//!   PROBE-OK   the last probe was answered but carried no windows
//!   BLOCKED    the 5-hour window is spent (or at its ceiling), back within hours
//!   SPENT      the week is spent, refused, or at its ceiling
//!   PROBE-?    never probed, or the reading is too old to trust; also a capped
//!              account with no current reading of one of its ceiling windows
//!   PROBE-ERR  the stored one-year token was refused outright (401/403)
//!   STALE      a saved login older than STALE_DAYS, whose refresh token has
//!              probably been rotated by whatever else used the account
//!   EXPIRED    the saved login itself has lapsed; switching can only fail
//!
//! `fresh` and `PROBE-OK` are the candidates `best --switch` takes. A saved
//! login that holds a usable one-year token never gets STALE or EXPIRED. The
//! rank is the binding constraint, max(week, 5h) used; blocked rows come after
//! every open one and stale/expired logins last.

use crate::avail::{
    Standing, age_ms, alias_emails, clock_ms, fmt_age, max_age_env, read_availability, standing,
    usable_tokens, windows_for,
};
use crate::caps::{Hit, cap_label, caps_view};
use crate::util::{
    env_or_empty, iso_ms, js_number, js_round, list_dir, mtime_ms, num, read_json, to_fixed,
};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

const UNREAD_STATE: &str = "PROBE-?";

fn order_of(state: &str) -> i32 {
    match state {
        "fresh" => 0,
        "PROBE-OK" => 1,
        "BLOCKED" => 2,
        "PROBE-?" | "PROBE-ERR" => 3,
        "SPENT" => 4,
        "STALE" => 5,
        _ => 6,
    }
}

/// An account at or over a ceiling reads as the provider's own refusal would:
/// BLOCKED for a 5-hour ceiling, SPENT for a weekly one.
fn cap_state_of(hit: Option<&Hit>) -> Option<&'static str> {
    hit.map(|h| if h.window == "7d" { "SPENT" } else { "BLOCKED" })
}

pub struct Pick {
    pub id: String,
    pub weekly: Option<i64>,
    pub reset: String,
    pub age_days: Option<f64>,
    pub state: &'static str,
    pub session: Option<i64>,
    binding: i64,
    order: i32,
    cap: Option<Hit>,
}

/// Ranked rows, or None when no usable ceiling policy resolved (the notes and
/// the refusal have then been written to stderr).
pub fn rank(store: &Path, now: f64) -> Result<(Vec<Pick>, crate::caps::CapsView), ()> {
    let stale_days = js_number(&env_or_empty("STALE_DAYS"));
    let stale_ms = (if stale_days.is_nan() || stale_days == 0.0 {
        7.0
    } else {
        stale_days
    }) * 86_400_000.0;
    let max_age = max_age_env();

    let mut captured: Vec<(String, f64)> = Vec::new();
    let mut login_expired: Vec<(String, bool)> = Vec::new();
    for f in list_dir(store) {
        let Some(alias) = f.strip_suffix(".creds.json") else {
            continue;
        };
        let path = store.join(&f);
        let Some(m) = mtime_ms(&path) else { continue };
        captured.push((alias.to_string(), (now - m) / 86_400_000.0));
        let exp = read_json(&path).and_then(|v| {
            v.get("claudeAiOauth")?
                .get("refreshTokenExpiresAt")
                .and_then(num)
        });
        if let Some(exp) = exp.filter(|e| *e > 0.0) {
            login_expired.push((alias.to_string(), exp <= now));
        }
    }

    let emails = alias_emails(store);
    let avail = read_availability(store);
    let tokens = usable_tokens(store, now);

    let caps = caps_view(store, now, max_age, Some(&avail));
    if caps.unusable.is_some() {
        for n in &caps.notes {
            eprintln!("note: {n}");
        }
        eprintln!("best: no valid account ceiling policy resolved, refusing to rank accounts");
        return Err(());
    }

    let mut ids: BTreeSet<String> = captured.iter().map(|(a, _)| a.clone()).collect();
    ids.extend(tokens.iter().cloned());
    let mut picks = Vec::new();
    for id in ids {
        let has_token = tokens.contains(&id);
        let windows = windows_for(&id, &avail, &emails, now, max_age);
        let week = windows.get("7d");
        let sess = windows.get("5h");
        let weekly = week.map(|w| js_round(w.used_pct).max(0.0) as i64);
        let session = sess.map(|w| js_round(w.used_pct).max(0.0) as i64);
        let entry = avail.get(&id);
        let held = standing(entry, now, max_age);
        let age = captured.iter().find(|(a, _)| *a == id).map(|(_, d)| *d);
        let expired = !has_token && login_expired.iter().any(|(a, e)| *a == id && *e);
        let stale = !has_token && !expired && age.is_some_and(|d| d * 86_400_000.0 >= stale_ms);
        let reason = entry.and_then(|e| e.get("reason")).and_then(Value::as_str);
        let refused_week = held == Standing::Out && matches!(reason, Some("week") | Some("model"));
        let refused_5h = held == Standing::Out && reason == Some("5h");
        let cap = caps.over_hit(&id).cloned();
        let capped = cap_state_of(cap.as_ref());
        // Judged on the figure as measured: 99.6% rounds to 100 in the table, but
        // the server still answers. Use an account until it is refused.
        let spent = !expired
            && !stale
            && (week.is_some_and(|w| w.used_pct >= 100.0)
                || refused_week
                || capped == Some("SPENT"));
        let blocked = !expired
            && !stale
            && (sess.is_some_and(|w| w.used_pct >= 100.0)
                || refused_5h
                || capped == Some("BLOCKED"));
        // A token the probe just saw refused (401/403) cannot authenticate. A
        // failure that says nothing about the token (no reply, 5xx) does not count.
        let rec = entry.filter(|e| e.is_object());
        let http = rec.and_then(|r| r.get("http")).and_then(num);
        let dead_token = has_token
            && rec.and_then(|r| r.get("state")).and_then(Value::as_str) == Some("unknown")
            && matches!(http, Some(h) if h == 401.0 || h == 403.0)
            && rec.and_then(|r| r.get("cred")).and_then(Value::as_str) == Some("token")
            && age_ms(rec, now).unwrap_or(f64::INFINITY) < max_age;
        let measured = !windows.is_empty();
        let state = if expired {
            "EXPIRED"
        } else if stale {
            "STALE"
        } else if dead_token {
            "PROBE-ERR"
        } else if spent {
            "SPENT"
        } else if blocked {
            "BLOCKED"
        } else if caps.unread_of(&id).is_some() {
            UNREAD_STATE
        } else if measured {
            "fresh"
        } else if held == Standing::Ok {
            "PROBE-OK"
        } else {
            UNREAD_STATE
        };
        let seen = age_ms(rec, now);
        let age_days = match seen {
            Some(s) => Some(s / 86_400_000.0),
            None => age,
        };
        let reset = match week.and_then(|w| w.reset_ms).filter(|r| *r != 0.0) {
            Some(r) => iso_ms(r),
            None => "-".into(),
        };
        picks.push(Pick {
            binding: weekly.unwrap_or(0).max(session.unwrap_or(0)),
            order: order_of(state),
            id,
            weekly,
            reset,
            age_days,
            state,
            session,
            cap,
        });
    }
    picks.sort_by(|a, b| a.order.cmp(&b.order).then(a.binding.cmp(&b.binding)));
    Ok((picks, caps))
}

/// The ranked rows as `best --raw` prints them; the ceiling legend goes to
/// stderr as it is built. Err(3) means the refusal is already explained.
pub fn best(store: &Path) -> Result<String, i32> {
    let Ok((picks, caps)) = rank(store, clock_ms()) else {
        return Err(3);
    };
    let (rows, legend) = render(&picks, &caps);
    for line in legend {
        eprintln!("{line}");
    }
    Ok(rows)
}

/// The stdout rows and the stderr legend lines for ranked picks.
fn render(picks: &[Pick], caps: &crate::caps::CapsView) -> (String, Vec<String>) {
    let mut out = String::new();
    for p in picks {
        let age = p.age_days.map_or("-".to_string(), |d| to_fixed(d, 1));
        let dash = |v: Option<i64>| v.map_or("-".to_string(), |n| n.to_string());
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\n",
            p.id,
            dash(p.weekly),
            p.reset,
            age,
            p.state,
            dash(p.session)
        ));
    }
    let mut legend: Vec<String> = caps.notes.iter().map(|n| format!("note: {n}")).collect();
    let path = caps.path.clone().unwrap_or_default();
    for p in picks {
        if p.state == UNREAD_STATE
            && let Some(blind) = caps.unread_of(&p.id)
        {
            legend.push(format!(
                    "cap: {} has a ceiling and no current reading of its {} window per {path}; shown {}, so --switch passes it over. Read it with: sharkswap probe {}",
                    p.id,
                    blind.join(" and "),
                    p.state,
                    p.id
                ));
        }
        let Some(cap) = &p.cap else { continue };
        if Some(p.state) != cap_state_of(Some(cap)) {
            continue;
        }
        legend.push(format!(
            "cap: {} is at its ceiling, {} per {path}; shown {}, so --switch passes it over",
            p.id,
            cap_label(cap),
            p.state
        ));
    }
    (out, legend)
}

/// Offline alias-list usage cells: `<alias>\t<5h%>/<week%>`, one line per alias
/// that has a current reading.
pub fn list_cells(store: &Path) -> Vec<(String, String)> {
    let now = clock_ms();
    let max_age = max_age_env();
    let avail = read_availability(store);
    let emails = alias_emails(store);
    let pct = |w: Option<&crate::avail::Window>| -> String {
        let Some(w) = w else { return "-".into() };
        let rounded = js_round(w.used_pct * 10.0) / 10.0;
        if rounded >= 100.0 && w.used_pct < 100.0 {
            "99.9".into()
        } else {
            rounded.to_string()
        }
    };
    let mut out = Vec::new();
    for (alias, _) in emails.iter() {
        let windows = windows_for(alias, &avail, &emails, now, max_age);
        if windows.is_empty() {
            continue;
        }
        out.push((
            alias.clone(),
            format!("{}/{}", pct(windows.get("5h")), pct(windows.get("7d"))),
        ));
    }
    out
}

/// One status-bar line for one alias, from the store. Never touches a provider:
/// a status bar redraws every few seconds.
pub fn bar(store: &Path, alias: &str) -> String {
    let now = clock_ms();
    let max_age = max_age_env();
    let avail = read_availability(store);
    let emails = alias_emails(store);
    let windows = windows_for(alias, &avail, &emails, now, max_age);
    let pct = |w: &crate::avail::Window| format!("{}%", js_round(w.used_pct).max(0.0));
    let mut parts: Vec<String> = vec![alias.to_string()];
    let sess = windows.get("5h");
    let week = windows.get("7d");
    if let Some(s) = sess {
        parts.push(format!("5h {}", pct(s)));
    }
    if let Some(w) = week {
        parts.push(format!("wk {}", pct(w)));
    }
    let entry = avail.get(alias);
    let age = age_ms(entry, now);
    if sess.is_some() || week.is_some() {
        if let Some(a) = age.filter(|a| *a > 3.0 * 3_600_000.0) {
            parts.push(format!("{} old", fmt_age(Some(a))));
        }
    } else {
        let held = standing(entry, now, max_age);
        match held {
            Standing::Ok => parts.push("answered".into()),
            Standing::Out => {
                let reason = match entry.and_then(|e| e.get("reason")).and_then(Value::as_str) {
                    Some("week") => "wk",
                    Some("5h") => "5h",
                    Some("model") => "model",
                    _ => "",
                };
                parts.push(format!("refused {reason}").trim().to_string());
            }
            Standing::None => parts.push("no reading".into()),
        }
        if held != Standing::None
            && let Some(a) = age
        {
            parts.push(format!("{} ago", fmt_age(Some(a))));
        }
    }
    let creds = store.join(format!("{alias}.creds.json"));
    if creds.exists() {
        let exp = read_json(&creds)
            .and_then(|v| {
                v.get("claudeAiOauth")?
                    .get("refreshTokenExpiresAt")
                    .and_then(num)
            })
            .filter(|e| *e > 0.0);
        if let Some(exp) = exp {
            let d = (exp - now) / 86_400_000.0;
            parts.push(if d <= 0.0 {
                "login EXPIRED".to_string()
            } else if d < 1.0 {
                format!("login {}h", js_round(d * 24.0).max(1.0))
            } else {
                format!("login {}d", js_round(d))
            });
        }
    }
    // Silent on a table error: a status bar has no room for one, and `best` says it.
    let view = caps_view(store, now, max_age, Some(&avail));
    if let Some(cap) = view.over_hit(alias) {
        parts.push(format!(
            "cap {}",
            if cap.window == "7d" { "wk" } else { cap.window }
        ));
    }
    parts.join(" \u{b7} ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{H, NOW, TestStore, meta, reading, sec, touch};
    use serde_json::json;
    use std::collections::HashMap;

    /// The `best --raw` rows of a ranked store, and the stderr legend, at `NOW`.
    fn ranked(dir: &Path) -> Result<(Vec<Vec<String>>, Vec<String>), ()> {
        let (picks, caps) = rank(dir, NOW)?;
        let (rows, legend) = render(&picks, &caps);
        let rows = rows
            .lines()
            .map(|l| l.split('\t').map(str::to_string).collect())
            .collect();
        Ok((rows, legend))
    }

    fn states(rows: &[Vec<String>]) -> HashMap<&str, &str> {
        rows.iter()
            .map(|r| (r[0].as_str(), r[4].as_str()))
            .collect()
    }

    fn creds() -> serde_json::Value {
        json!({ "claudeAiOauth": {} })
    }

    #[test]
    fn best_ranks_by_the_tighter_window_and_classifies_every_state() {
        let mut spent = reading(0.1, 0.99);
        spent["state"] = json!("out");
        spent["reason"] = json!("week");
        spent["resets"] = json!(sec(NOW + 24.0 * H));
        let dir = TestStore::new(&[
            ("fresh.json", meta("1@example.com")),
            ("fresh.creds.json", creds()),
            ("half.json", meta("2@example.com")),
            ("half.creds.json", creds()),
            ("spent.json", meta("3@example.com")),
            ("spent.creds.json", creds()),
            ("never.json", meta("4@example.com")),
            ("never.creds.json", creds()),
            ("lapsed.json", meta("5@example.com")),
            (
                "lapsed.creds.json",
                json!({ "claudeAiOauth": { "refreshTokenExpiresAt": NOW - H } }),
            ),
            (
                "tokens/tok.json",
                json!({ "token": "t", "created": sec(NOW) - 100 }),
            ),
            ("tok.creds.json", creds()),
            (
                "availability.json",
                json!({
                    "fresh": reading(0.1, 0.2),
                    "half": reading(0.5, 0.6),
                    "spent": spent,
                    "tok": { "state": "unknown", "reason": "unknown", "at": sec(NOW) - 10, "via": "headers", "http": 401, "cred": "token" },
                }),
            ),
        ]);
        let (rows, _) = ranked(&dir).unwrap();
        let by = states(&rows);
        assert_eq!(by["fresh"], "fresh");
        assert_eq!(by["half"], "fresh");
        assert_eq!(by["never"], "PROBE-?");
        assert_eq!(by["spent"], "SPENT");
        assert_eq!(by["lapsed"], "EXPIRED");
        assert_eq!(by["tok"], "PROBE-ERR");
        let order: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
        let at = |a: &str| order.iter().position(|x| *x == a).unwrap();
        assert!(at("fresh") < at("half"));
        assert!(at("half") < at("spent"));
        assert_eq!(order.last(), Some(&"lapsed"), "dead logins last");
    }

    #[test]
    fn best_marks_a_login_older_than_stale_days_stale_unless_a_one_year_token_backs_it() {
        let dir = TestStore::new(&[
            ("old.json", meta("1@example.com")),
            ("old.creds.json", creds()),
            ("old-tok.json", meta("2@example.com")),
            ("old-tok.creds.json", creds()),
            (
                "tokens/old-tok.json",
                json!({ "token": "t", "created": sec(NOW) - 100 }),
            ),
        ]);
        for name in ["old.creds.json", "old-tok.creds.json"] {
            touch(&dir.join(name), NOW - 30.0 * 86_400_000.0);
        }
        let (rows, _) = ranked(&dir).unwrap();
        let by = states(&rows);
        assert_eq!(by["old"], "STALE");
        assert_ne!(by["old-tok"], "STALE");
    }

    #[test]
    fn best_reads_ceilings_as_the_providers_own_refusal_and_an_unusable_table_ranks_nothing() {
        let mut files = vec![
            ("a.json", meta("1@example.com")),
            ("a.creds.json", creds()),
            ("b.json", meta("2@example.com")),
            ("b.creds.json", creds()),
            (
                "availability.json",
                json!({ "a": reading(0.9, 0.3), "b": reading(0.2, 0.95) }),
            ),
        ];
        files.push((
            "account-caps.json",
            json!({ "caps": { "a": { "5h": 80 }, "b": { "7d": 90 } } }),
        ));
        let dir = TestStore::new(&files);
        let (rows, legend) = ranked(&dir).unwrap();
        let by = states(&rows);
        assert_eq!(
            by["a"], "BLOCKED",
            "a 5h ceiling reads as a spent 5h window"
        );
        assert_eq!(by["b"], "SPENT", "a weekly ceiling reads as a spent week");
        assert!(
            legend
                .iter()
                .any(|l| l.starts_with("cap: a is at its ceiling")),
            "{legend:?}"
        );
        files.pop();
        files.push(("account-caps.json", json!("{")));
        let dir = TestStore::new(&files);
        assert!(ranked(&dir).is_err());
    }
}
