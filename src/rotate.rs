//! `sharkswap rotate`: when does a host's global Claude login move to another
//! account, and to which one. The decision is one pure function (`decide`) so
//! every rule can be tested with plain values; the rest of this file is the
//! command a timer runs: read the current account's rate-limit headers with the
//! probe's own request, read the pool's only when a move is needed, switch by
//! running `sharkswap <alias> --force` as an operator would, and check the
//! login by reading the alias back.
//!
//! The rules:
//!
//!  1. Use an account until it is REFUSED (its 7d or 5h status is "rejected"),
//!     then move. No percentage threshold: the header figures read a few points
//!     above the claude.ai usage page, so a threshold would abandon accounts
//!     that still have room. Only those two windows count: a model-scoped one
//!     (7d_opus, 7d_sonnet) never moves a host.
//!  2. EXCEPT an account with a ceiling (account-caps.json): it counts as blocked
//!     once its 5-hour or weekly utilization reaches the ceiling, and it is left
//!     early when the host's burn rate projects it over its ceiling before the
//!     next tick. The ceiling is a promise to whoever shares the account. The
//!     rate is the one measured on the current account since the previous run,
//!     or, when that run was another account, the host rate the state kept
//!     (good for STALE_TICKS ticks): a candidate that sits just under its ceiling
//!     is skipped when the host would burn through it before the next run. The
//!     current account is CAPPED when any figure it has, projected one tick
//!     ahead, reaches its ceiling, whatever else is missing from the reading;
//!     only a hard 5h or 7d "rejected" takes that away (the account is then
//!     refused, not capped).
//!  3. At most one switch per min-gap. Leaving a capped account never waits. An
//!     account that cannot be read is not left on the first run that fails (a
//!     single failed request is not a refusal): it takes UNREADABLE_RUNS
//!     unreadable runs in a row, or a refused credential (401/403) at once. A
//!     reading that lacks a ceilinged figure is unreadable in that sense only
//!     while the figures it does have are under their ceilings.
//!  4. Candidates come from the host's pool file. Rank = lowest max(5h, week),
//!     then lower 5h, then pool order. Blocked and capped candidates are skipped,
//!     and so is one whose ceilinged window has no figure. A target whose switch
//!     failed within the last STALE_TICKS ticks is ranked after every other
//!     candidate rather than excluded.
//!  5. When the pool has nothing open and the current account is capped, any
//!     token account under its own ceiling may be used.
//!  6. When nothing readable is open but a pool alias holds only a stored login
//!     whose access token has expired (so it cannot be probed; claude refreshes
//!     it on first use), that alias is a last resort, but ONLY when the best
//!     readable candidate is none or at 0.95 or more, and only when something
//!     answered this run.
//!
//! Utilizations are the header fractions (0..1). A ceiling is a percent, and a
//! figure is compared the way every other view compares one: rounded to a whole
//! percent. Times are epoch seconds throughout (the state file's unit).

use crate::caps::{Caps, ceiling_of, effective_caps};
use crate::probe::{Cred, Reply, ask, file_login, probe_cred, probe_model};
use crate::store::{COMMAND_WORDS, Ctx, Spelling, alias_ok, alias_spelling, valid_oauth_file};
use crate::util::*;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{CString, OsString};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

pub const DEFAULT_TICK: f64 = 600.0; // seconds between two runs of the host timer
pub const DEFAULT_MIN_GAP: f64 = 900.0; // seconds between two switches
pub const STALE_TICKS: f64 = 3.0; // a last reading older than this many ticks says nothing about the rate
pub const LAST_RESORT_BELOW: f64 = 0.95;
pub const UNREADABLE_RUNS: f64 = 2.0; // unreadable runs in a row before the current account is left

/// The key the state files an account under when the host's login matches no
/// stored account (the rate is still the host's, and still worth measuring).
const UNKNOWN: &str = "_unknown";

// ---- numbers as JavaScript prints them --------------------------------------

/// `String(x)` for a number: shortest round-trip digits, no exponent between
/// 1e-6 and 1e21.
pub fn js_str(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    if x == 0.0 {
        return "0".into();
    }
    let sign = if x < 0.0 { "-" } else { "" };
    let sci = format!("{:e}", x.abs());
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exp.parse::<i32>().unwrap_or(0) + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let sign_e = if e < 0 { "-" } else { "+" };
        if k == 1 {
            format!("{digits}e{sign_e}{}", e.abs())
        } else {
            format!("{}.{}e{sign_e}{}", &digits[..1], &digits[1..], e.abs())
        }
    };
    format!("{sign}{body}")
}

fn fmt(v: Option<f64>) -> String {
    v.map_or_else(|| "?".to_string(), js_str)
}

fn finite(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|x| x.is_finite())
}

// ---- readings -----------------------------------------------------------------

/// One account's readable state, from a probe: the 5h and week fractions and
/// their header statuses. An account that could not be read has no `Reading`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reading {
    pub five: Option<f64>,
    pub week: Option<f64>,
    pub five_status: String,
    pub week_status: String,
}

/// Refused = the 5h or the 7d window itself says "rejected". A model-scoped
/// window (7d_opus, 7d_sonnet) is not one of these: it does not move a host.
pub fn refused(r: &Reading) -> bool {
    r.week_status == "rejected" || r.five_status == "rejected"
}

/// Utilization per second, per window.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rate {
    pub five: Option<f64>,
    pub week: Option<f64>,
}

/// The reading kept by the previous run.
#[derive(Clone, Debug, Default)]
pub struct Last {
    pub alias: Option<String>,
    pub t: Option<f64>,
    pub five: Option<f64>,
    pub week: Option<f64>,
}

/// The host's burn rate kept in the state.
#[derive(Clone, Debug, Default)]
pub struct HostRate {
    pub five: Option<f64>,
    pub week: Option<f64>,
    pub t: Option<f64>,
}

pub type Readings = BTreeMap<String, Option<Reading>>;

fn missing_ceiling_figure(reading: Option<&Reading>, alias: Option<&str>, caps: &Caps) -> bool {
    let Some(cap) = alias.and_then(|a| ceiling_of(caps, a)) else {
        return false;
    };
    (cap.h5.is_some() && reading.and_then(|r| r.five).is_none())
        || (cap.d7.is_some() && reading.and_then(|r| r.week).is_none())
}

/// Utilization per second on the current account since the previous run, per
/// window, from the reading kept then: none when it was another account, when it
/// is older than STALE_TICKS ticks or not older than now at all, and a window
/// whose figure did not rise (or fell: its window reset) has no rate.
pub fn rate_since(
    last: Option<&Last>,
    current: &str,
    reading: Option<&Reading>,
    now: f64,
    tick: f64,
) -> Option<Rate> {
    let (last, reading) = (last?, reading?);
    if last.alias.as_deref() != Some(current) {
        return None;
    }
    let dt = now - last.t.unwrap_or(0.0);
    if !(dt > 0.0 && dt <= STALE_TICKS * tick) {
        return None;
    }
    let mut out = Rate::default();
    if let (Some(a), Some(b)) = (last.five, reading.five)
        && b > a
    {
        out.five = Some((b - a) / dt);
    }
    if let (Some(a), Some(b)) = (last.week, reading.week)
        && b > a
    {
        out.week = Some((b - a) / dt);
    }
    Some(out)
}

/// The host's burn rate kept in the state, when it is fresh enough to say
/// anything about now.
fn fresh_host_rate(host: Option<&HostRate>, now: f64, tick: f64) -> Option<Rate> {
    let h = host?;
    let dt = now - h.t.unwrap_or(f64::NEG_INFINITY);
    if !(dt >= 0.0 && dt <= STALE_TICKS * tick) {
        return None;
    }
    Some(Rate {
        five: Some(h.five.unwrap_or(0.0)),
        week: Some(h.week.unwrap_or(0.0)),
    })
}

/// Whether a figure the reading HAS is at the alias's ceiling, projected one tick
/// ahead by `rate`. A figure the reading lacks says nothing either way. The
/// figure is compared rounded to a whole percent, the way every other view
/// shows it.
fn ceiling_hit(
    reading: Option<&Reading>,
    alias: Option<&str>,
    caps: &Caps,
    rate: Option<Rate>,
    tick: f64,
) -> bool {
    let Some(cap) = alias.and_then(|a| ceiling_of(caps, a)) else {
        return false;
    };
    let Some(reading) = reading else { return false };
    let over = |u: Option<f64>, c: Option<f64>, per_second: Option<f64>| match (u, c) {
        (Some(now), Some(c)) => js_round((now + per_second.unwrap_or(0.0) * tick) * 100.0) >= c,
        _ => false,
    };
    over(reading.five, cap.h5, rate.and_then(|r| r.five))
        || over(reading.week, cap.d7, rate.and_then(|r| r.week))
}

/// Blocked = refused, or at the alias's ceiling.
pub fn blocked(reading: &Reading, alias: &str, caps: &Caps, rate: Option<Rate>, tick: f64) -> bool {
    refused(reading) || ceiling_hit(Some(reading), Some(alias), caps, rate, tick)
}

/// How the host's own account stands, from its reading.
struct Stand {
    refused: bool,
    ceiling_hit: bool,
    /// The reading says something firm: refused, at the ceiling, or under it with
    /// every ceilinged figure present. Anything else counts as unreadable.
    usable: bool,
}

fn stand(
    reading: Option<&Reading>,
    alias: Option<&str>,
    caps: &Caps,
    rate: Option<Rate>,
    tick: f64,
) -> Stand {
    let Some(r) = reading else {
        return Stand {
            refused: false,
            ceiling_hit: false,
            usable: false,
        };
    };
    let is_refused = refused(r);
    let hit = ceiling_hit(Some(r), alias, caps, rate, tick);
    let missing = missing_ceiling_figure(Some(r), alias, caps);
    Stand {
        refused: is_refused,
        ceiling_hit: hit,
        usable: is_refused || hit || !missing,
    }
}

/// Aliases whose switch failed within the last STALE_TICKS ticks, from the
/// state's `failed` map and its older single `last_failed`: alias to time, in
/// first-seen order. An entry from the future is not trusted.
pub fn recent_failures(
    failed: &[(String, f64)],
    last_failed: Option<&(String, f64)>,
    now: f64,
    tick: f64,
) -> Vec<(String, f64)> {
    let mut out: Vec<(String, f64)> = Vec::new();
    let mut add = |alias: &str, t: f64| {
        if alias.is_empty() || !t.is_finite() {
            return;
        }
        let age = now - t;
        if age >= 0.0 && age <= STALE_TICKS * tick {
            match out.iter_mut().find(|(a, _)| a == alias) {
                Some(slot) => {
                    if slot.1 <= t {
                        slot.1 = t;
                    }
                }
                None => out.push((alias.to_string(), t)),
            }
        }
    };
    for (alias, t) in failed {
        add(alias, *t);
    }
    if let Some((alias, t)) = last_failed {
        add(alias, *t);
    }
    out
}

struct Pick {
    alias: String,
    score: (f64, f64),
}

struct Ranked {
    best: Option<Pick>,
    later: Option<Pick>,
    unreadable: Vec<String>,
    deferred: Vec<String>,
}

/// The open accounts among `aliases` with the most room, and the ones that could
/// not be read. An alias `readings` does not mention has no credential here and
/// is not a candidate at all. An alias in `failed` is ranked after every other:
/// `best` is the best of the rest, `later` the best of those, `deferred` names
/// the open ones among them.
fn rank(
    aliases: &[String],
    readings: &Readings,
    caps: &Caps,
    current: Option<&str>,
    rate: Option<Rate>,
    tick: f64,
    failed: &HashSet<String>,
) -> Ranked {
    let mut out = Ranked {
        best: None,
        later: None,
        unreadable: Vec::new(),
        deferred: Vec::new(),
    };
    let better = |score: (f64, f64), held: &Option<Pick>| match held {
        None => true,
        Some(h) => score.0 < h.score.0 || (score.0 == h.score.0 && score.1 < h.score.1),
    };
    for alias in aliases {
        if Some(alias.as_str()) == current {
            continue;
        }
        let Some(entry) = readings.get(alias) else {
            continue;
        };
        let Some(p) = entry else {
            out.unreadable.push(alias.clone());
            continue;
        };
        if blocked(p, alias, caps, rate, tick) {
            continue;
        }
        // A ceilinged window with no figure cannot be shown to be under its ceiling.
        if missing_ceiling_figure(Some(p), Some(alias), caps) {
            continue;
        }
        // The tighter of the two limits, so an account with an empty 5h window but a
        // nearly spent week (5h at 0.05, week at 0.99) loses to one with room.
        let five = p.five.unwrap_or(0.0);
        let week = p.week.unwrap_or(0.0);
        let score = (five.max(week), five);
        if failed.contains(alias) {
            out.deferred.push(alias.clone());
            if better(score, &out.later) {
                out.later = Some(Pick {
                    alias: alias.clone(),
                    score,
                });
            }
        } else if better(score, &out.best) {
            out.best = Some(Pick {
                alias: alias.clone(),
                score,
            });
        }
    }
    out
}

// ---- the decision -------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Input {
    /// Alias the host is logged in as, or None when unknown.
    pub current: Option<String>,
    /// That account's reading, or None when it could not be read.
    pub reading: Option<Reading>,
    /// Kept by the previous run.
    pub last: Option<Last>,
    /// Aliases this host may move to, in file order.
    pub pool: Vec<String>,
    /// The pool's readings; an alias absent has no credential here. None = not read yet.
    pub candidates: Option<Readings>,
    /// "token" or "snapshot": the credential behind each reading (default "token").
    pub kinds: HashMap<String, String>,
    /// Pool aliases holding only a stored login that cannot be probed (rule 6).
    pub last_resort: Vec<String>,
    /// Token accounts outside the pool (rule 5 only). None = not read yet.
    pub tokens: Option<Readings>,
    pub caps: Caps,
    pub now: f64,
    pub last_switch: f64,
    pub min_gap: f64,
    pub tick: f64,
    pub host_rate: Option<HostRate>,
    /// Runs in a row, before this one, that could not read the current account.
    pub unreadable: f64,
    /// The current account's credential was refused (401/403), or there is no login at all.
    pub denied: bool,
    /// Nothing about the current account answered this run.
    pub no_reply: bool,
    /// Some pool candidate answered this run.
    pub answered_candidates: bool,
    pub failed: Vec<(String, f64)>,
    pub last_failed: Option<(String, f64)>,
}

impl Default for Input {
    fn default() -> Input {
        Input {
            current: None,
            reading: None,
            last: None,
            pool: Vec::new(),
            candidates: None,
            kinds: HashMap::new(),
            last_resort: Vec::new(),
            tokens: None,
            caps: Caps::default(),
            now: f64::NAN,
            last_switch: 0.0,
            min_gap: DEFAULT_MIN_GAP,
            tick: DEFAULT_TICK,
            host_rate: None,
            unreadable: 0.0,
            denied: false,
            no_reply: false,
            answered_candidates: false,
            failed: Vec::new(),
            last_failed: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do (the current account is open).
    Stay,
    /// It must move but cannot: the gap has not passed, or no account is open.
    Wait,
    /// Move to `target`; `kind` is the credential it will land on.
    Switch,
    /// The decision needs readings not supplied yet (`need`).
    Need,
}

#[derive(Clone, Debug)]
pub struct Decision {
    pub action: Action,
    pub target: Option<String>,
    pub kind: Option<String>,
    pub need: Option<&'static str>,
    /// Log text; may hold several lines.
    pub reason: String,
}

impl Decision {
    fn plain(action: Action, reason: String) -> Decision {
        Decision {
            action,
            target: None,
            kind: None,
            need: None,
            reason,
        }
    }
}

pub fn decide(input: &Input) -> Result<Decision, String> {
    if !input.now.is_finite() {
        return Err("decide: now must be a number".into());
    }
    let (now, tick) = (input.now, input.tick);
    let current = input.current.as_deref();
    let reading = input.reading.as_ref();
    let caps = &input.caps;

    // The rate on the current account since the previous run when that run saw
    // this same account; otherwise the host's rate from the state. A login that is
    // unknown (current None) is still one account to the rate.
    let rate = rate_since(
        input.last.as_ref(),
        current.unwrap_or(UNKNOWN),
        reading,
        now,
        tick,
    )
    .or_else(|| fresh_host_rate(input.host_rate.as_ref(), now, tick));
    let st = stand(reading, current, caps, rate, tick);
    let current_reading = if st.usable { reading } else { None };
    if let Some(r) = current_reading
        && !st.refused
        && !st.ceiling_hit
    {
        return Ok(Decision::plain(
            Action::Stay,
            format!(
                "ok: {} week {} 5h {}; no change",
                current.unwrap_or("null"),
                fmt(r.week),
                fmt(r.five)
            ),
        ));
    }
    // Left early because of a ceiling: a figure it has is at or projected over its
    // ceiling, and neither window is rejected. Only this kind of leaving skips the
    // switch gap and may go outside the pool.
    let capped = st.ceiling_hit && !st.refused;
    let name = current.unwrap_or("unknown account");
    let how = match current_reading {
        Some(r) => format!("week {} 5h {}", fmt(r.week), fmt(r.five)),
        None if input.denied => "credential refused".to_string(),
        None => "unreadable".to_string(),
    };
    let cur_desc = format!(
        "{name} ({how}{})",
        if capped { ", at its ceiling" } else { "" }
    );

    // One failed request is not a refusal: an account that cannot be read is kept
    // until it has failed UNREADABLE_RUNS runs in a row. A refused credential
    // (401/403, or no login at all) does not wait.
    if current_reading.is_none() && !input.denied {
        let n = input.unreadable + 1.0;
        if n < UNREADABLE_RUNS {
            return Ok(Decision::plain(
                Action::Wait,
                format!(
                    "cannot read {name} (run {} of {}); it moves only when the next run cannot read it either",
                    js_str(n),
                    js_str(UNREADABLE_RUNS)
                ),
            ));
        }
    }
    if !capped && now - input.last_switch < input.min_gap {
        return Ok(Decision::plain(
            Action::Wait,
            format!(
                "blocked on {name} but last switch was under {} min ago; waiting",
                js_str((input.min_gap / 60.0).floor())
            ),
        ));
    }
    let Some(candidates) = &input.candidates else {
        return Ok(Decision {
            action: Action::Need,
            target: None,
            kind: None,
            need: Some("pool"),
            reason: format!("{name} must move; reading the pool"),
        });
    };

    let mut pool_aliases: Vec<String> = Vec::new();
    for a in &input.pool {
        if !pool_aliases.contains(a) {
            pool_aliases.push(a.clone());
        }
    }
    let mut lines: Vec<String> = Vec::new();
    let failed_at = recent_failures(&input.failed, input.last_failed.as_ref(), now, tick);
    let failed_set: HashSet<String> = failed_at.iter().map(|(a, _)| a.clone()).collect();
    let first = rank(
        &pool_aliases,
        candidates,
        caps,
        current,
        rate,
        tick,
        &failed_set,
    );
    let unreadable = first.unreadable;
    let mut deferred = first.deferred;
    let (mut best, mut later) = (first.best, first.later);
    let (mut outside, mut later_outside) = (false, false); // the pick is a token account outside the pool
    let mut last_resort_alias: Option<String> = None;
    if best.is_none() && capped {
        // A ceiling is a promise to someone else: leave the account even when the
        // pool is empty, for any token account still under its own ceiling.
        let Some(tokens) = &input.tokens else {
            return Ok(Decision {
                action: Action::Need,
                target: None,
                kind: None,
                need: Some("tokens"),
                reason: format!(
                    "{name} is at its ceiling and no pool account is open; reading the token accounts"
                ),
            });
        };
        let others: Vec<String> = tokens
            .keys()
            .filter(|a| !pool_aliases.contains(a))
            .cloned()
            .collect();
        let t = rank(&others, tokens, caps, current, rate, tick, &failed_set);
        deferred.extend(t.deferred);
        if t.best.is_some() {
            best = t.best;
            outside = true;
        }
        if later.is_none() && t.later.is_some() {
            later = t.later;
            later_outside = true;
        }
    }
    // A saved login whose access token expired cannot be probed; claude refreshes
    // it on first use. Prefer it over a nearly spent account, and over none. Never
    // an account with a ceiling: it cannot be shown to be under it.
    let last_resort_from = |failed_ones: bool| -> Option<String> {
        unreadable
            .iter()
            .find(|a| {
                failed_set.contains(*a) == failed_ones
                    && input.last_resort.contains(a)
                    && ceiling_of(caps, a).is_none()
            })
            .cloned()
    };
    if best.as_ref().is_none_or(|b| b.score.0 >= LAST_RESORT_BELOW) {
        last_resort_alias = last_resort_from(false);
    }
    if best.is_none() && last_resort_alias.is_none() {
        // Nothing else is open: a target whose switch failed recently is ranked
        // last, not dropped. It may have failed for a reason that has passed, and
        // it may be the only way off a capped account.
        if later.is_some() {
            best = later;
            outside = later_outside;
        } else {
            last_resort_alias = last_resort_from(true);
        }
    }
    let target: Option<&str> = last_resort_alias
        .as_deref()
        .or(best.as_ref().map(|b| b.alias.as_str()));
    let target_failed = target.is_some_and(|t| failed_set.contains(t));
    if !deferred.is_empty() || target_failed {
        let mins = js_round(STALE_TICKS * tick / 60.0);
        let mut names: Vec<&str> = Vec::new();
        for n in deferred
            .iter()
            .map(String::as_str)
            .chain(target.filter(|_| target_failed))
        {
            if !names.contains(&n) {
                names.push(n);
            }
        }
        let used = match target {
            Some(t) if target_failed => format!("; {t} is used because no other account is open"),
            _ => String::new(),
        };
        lines.push(format!(
            "ranked last: the switch to {} failed within the last {} min{used}",
            names.join(", "),
            js_str(mins)
        ));
    }
    if let Some(alias) = last_resort_alias {
        if input.no_reply && !input.answered_candidates {
            return Ok(Decision::plain(
                Action::Wait,
                format!("blocked on {cur_desc}; no account answered this run; waiting"),
            ));
        }
        lines.push(format!("switch: {cur_desc} -> {alias} [snapshot, last resort: its saved login cannot be read yet]"));
        return Ok(Decision {
            action: Action::Switch,
            target: Some(alias),
            kind: Some("snapshot".into()),
            need: None,
            reason: lines.join("\n"),
        });
    }
    let Some(best) = best else {
        lines.push(format!(
            "blocked on {cur_desc}; no pool account open; waiting"
        ));
        return Ok(Decision::plain(Action::Wait, lines.join("\n")));
    };
    if outside {
        lines.push(format!(
            "{name} is at its ceiling and no pool account is open; leaving the pool for {}",
            best.alias
        ));
    }
    let kind = input
        .kinds
        .get(&best.alias)
        .cloned()
        .unwrap_or_else(|| "token".into());
    lines.push(format!(
        "switch: {cur_desc} -> {} [{kind}, tightest limit {}, 5h {}]",
        best.alias,
        js_str(best.score.0),
        js_str(best.score.1)
    ));
    Ok(Decision {
        action: Action::Switch,
        target: Some(best.alias),
        kind: Some(kind),
        need: None,
        reason: lines.join("\n"),
    })
}

// ---- readings from replies ------------------------------------------------------

/// One account's reading from the probe replies of one run. The 5h and 7d windows
/// are the account's, not a model's, so any reply carries them: each field comes
/// from the first reply that has it. A model-scoped window is not read. A refusal
/// of the credential (401/403) is no reading, and neither is a reply with neither
/// window.
pub fn reading_from_replies(replies: &[&Reply]) -> Option<Reading> {
    let ok: Vec<&&Reply> = replies
        .iter()
        .filter(|r| r.http != 401 && r.http != 403)
        .collect();
    let pick = |win: &str, field: &str| -> Option<&Value> {
        ok.iter().find_map(|r| {
            r.limits
                .get(win)
                .and_then(|w| w.get(field))
                .filter(|v| !v.is_null())
        })
    };
    let text = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or("").to_string();
    let reading = Reading {
        five: finite(pick("5h", "util")),
        week: finite(pick("7d", "util")),
        five_status: text(pick("5h", "status")),
        week_status: text(pick("7d", "status")),
    };
    if reading.five.is_none()
        && reading.week.is_none()
        && reading.five_status.is_empty()
        && reading.week_status.is_empty()
    {
        None
    } else {
        Some(reading)
    }
}

/// Neither request reached an HTTP response (or only a server fault came back).
fn no_reply_from(replies: &[&Reply]) -> bool {
    !replies.iter().any(|r| (100..500).contains(&r.http))
}

/// Whether any of these replies is the API refusing the credential itself
/// (401/403), as opposed to a request that merely failed.
pub fn denied_by_replies(replies: &[&Reply]) -> bool {
    replies.iter().any(|r| r.http == 401 || r.http == 403)
}

// ---- the state file -------------------------------------------------------------

fn alias_re_ok(a: &str) -> bool {
    !a.is_empty()
        && !a.starts_with('.')
        && a.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn state_map(path: &Path) -> Map<String, Value> {
    match read_json(path) {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

fn parse_last(v: Option<&Value>) -> Option<Last> {
    let o = v?.as_object()?;
    Some(Last {
        alias: o.get("alias").and_then(Value::as_str).map(str::to_string),
        t: finite(o.get("t")),
        five: finite(o.get("five")),
        week: finite(o.get("week")),
    })
}

fn parse_host_rate(v: Option<&Value>) -> Option<HostRate> {
    let o = v?.as_object()?;
    Some(HostRate {
        five: finite(o.get("five")),
        week: finite(o.get("week")),
        t: finite(o.get("t")),
    })
}

fn parse_failed(v: Option<&Value>) -> Vec<(String, f64)> {
    let Some(Value::Object(o)) = v else {
        return Vec::new();
    };
    o.iter()
        .filter_map(|(a, t)| finite(Some(t)).map(|t| (a.clone(), t)))
        .collect()
}

fn parse_last_failed(v: Option<&Value>) -> Option<(String, f64)> {
    let o = v?.as_object()?;
    Some((o.get("alias")?.as_str()?.to_string(), finite(o.get("t"))?))
}

/// Atomic where the directory allows it (temp file, then rename); a directory
/// that will not take the temp file or the rename falls back to writing the file
/// itself, so a state that can be written at all is never lost to that.
fn write_state(path: &Path, m: Map<String, Value>) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("rotate: {}", os_msg(&e));
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(fail)?;
    }
    let text = format!(
        "{}\n",
        serde_json::to_string(&Value::Object(m)).unwrap_or_default()
    );
    if write_atomic(path, text.as_bytes(), 0o600).is_ok() {
        return Ok(());
    }
    std::fs::write(path, text).map_err(fail)
}

fn reading_json(r: &Reading, key: &str, now: f64) -> Value {
    let mut o = Map::new();
    o.insert("alias".into(), Value::String(key.to_string()));
    o.insert("t".into(), json_number(now));
    o.insert("five".into(), r.five.map_or(Value::Null, json_number));
    o.insert("week".into(), r.week.map_or(Value::Null, json_number));
    Value::Object(o)
}

/// A run that read the account files its reading (and the host burn rate measured
/// against the previous one); a run that did not counts one more unreadable run
/// for it.
fn remember(
    path: &Path,
    key: &str,
    reading: Option<&Reading>,
    now: f64,
    tick: f64,
    caps: &Caps,
) -> Result<(), String> {
    let mut next = state_map(path);
    let last = parse_last(next.get("last_reading"));
    let measured = rate_since(last.as_ref(), key, reading, now, tick);
    // The same classification decide() makes of the reading, so a reading it would
    // call unreadable is counted as a missed run here too.
    let rate = measured
        .or_else(|| fresh_host_rate(parse_host_rate(next.get("host_rate")).as_ref(), now, tick));
    let usable = reading.filter(|r| stand(Some(r), Some(key), caps, rate, tick).usable);
    if let Some(r) = usable {
        next.insert("last_reading".into(), reading_json(r, key, now));
        // The host's burn rate, on whichever account the host was: measured
        // against the previous reading when that was the same account. A run that
        // measured none keeps the old one, which goes stale by its time.
        if let Some(m) = measured {
            let mut o = Map::new();
            o.insert("five".into(), json_number(m.five.unwrap_or(0.0)));
            o.insert("week".into(), json_number(m.week.unwrap_or(0.0)));
            o.insert("t".into(), json_number(now));
            next.insert("host_rate".into(), Value::Object(o));
        }
        next.shift_remove("unreadable");
    } else {
        let n = match next.get("unreadable").and_then(Value::as_object) {
            Some(prev) if prev.get("alias").and_then(Value::as_str) == Some(key) => {
                finite(prev.get("n")).map_or(1.0, |n| n + 1.0)
            }
            _ => 1.0,
        };
        let mut o = Map::new();
        o.insert("alias".into(), Value::String(key.to_string()));
        o.insert("n".into(), json_number(n));
        next.insert("unreadable".into(), Value::Object(o));
    }
    write_state(path, next)
}

/// `switched` (the host moved) or `failed` (a switch to `alias` did not take).
fn mark(path: &Path, switched: bool, alias: &str, now: f64, tick: f64) -> Result<(), String> {
    if !alias_re_ok(alias) {
        return Err(format!(
            "rotate: state {} needs --alias",
            if switched { "switched" } else { "failed" }
        ));
    }
    let mut next = state_map(path);
    let last_failed = parse_last_failed(next.get("last_failed"));
    let mut failed = recent_failures(
        &parse_failed(next.get("failed")),
        last_failed.as_ref(),
        now,
        tick,
    );
    next.shift_remove("last_failed");
    if switched {
        next.insert("last_switch".into(), json_number(now));
        next.insert("alias".into(), Value::String(alias.to_string()));
        next.shift_remove("unreadable");
        failed.retain(|(a, _)| a != alias);
    } else {
        match failed.iter_mut().find(|(a, _)| a == alias) {
            Some(slot) => slot.1 = now,
            None => failed.push((alias.to_string(), now)),
        }
    }
    if failed.is_empty() {
        next.shift_remove("failed");
    } else {
        next.insert(
            "failed".into(),
            Value::Object(
                failed
                    .into_iter()
                    .map(|(a, t)| (a, json_number(t)))
                    .collect(),
            ),
        );
    }
    write_state(path, next)
}

// ---- the run lock -----------------------------------------------------------------

/// One run at a time per state file, so a manual run and the timer never both
/// switch. An advisory lock on the lock file, which the system drops when the
/// process ends however it ends; the file is removed on release.
struct RunLock {
    file: Option<File>,
    path: PathBuf,
}

enum Lock {
    Held(RunLock),
    /// Another run holds it.
    Busy,
    /// The lock cannot be taken at all; the text says which step failed and why.
    Unavailable(String),
}

/// `Lock::Unavailable` for a failed step: what was tried, the system's message
/// and the errno.
fn unavailable(step: &str, path: &Path, e: &std::io::Error) -> Lock {
    let errno = e
        .raw_os_error()
        .map_or_else(|| "none".into(), |n| n.to_string());
    Lock::Unavailable(format!(
        "{step} {}: {} (errno {errno})",
        path.display(),
        os_msg(e)
    ))
}

fn take_lock(path: &Path) -> Lock {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty())
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        return unavailable("cannot create", dir, &e);
    }
    for _ in 0..5 {
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
        {
            Ok(f) => f,
            Err(e) => return unavailable("cannot open", path, &e),
        };
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Lock::Busy,
            Err(TryLockError::Error(e)) => return unavailable("cannot lock", path, &e),
        }
        // The previous holder removes the file as it lets go: a lock on a file the
        // path no longer names is a lock on nothing, so open the path again.
        let same = match (file.metadata(), std::fs::metadata(path)) {
            (Ok(a), Ok(b)) => a.ino() == b.ino() && a.dev() == b.dev(),
            _ => false,
        };
        if !same {
            continue;
        }
        let _ = file.set_len(0);
        let _ = (&file).write_all(format!("{} {}\n", std::process::id(), wall_secs()).as_bytes());
        if let Ok(c) = CString::new(path.as_os_str().as_bytes()) {
            let _ = LOCK_C.set(c);
            LOCK_HELD.store(true, Ordering::SeqCst);
        }
        return Lock::Held(RunLock {
            file: Some(file),
            path: path.to_path_buf(),
        });
    }
    Lock::Busy
}

impl Drop for RunLock {
    fn drop(&mut self) {
        LOCK_HELD.store(false, Ordering::SeqCst);
        rm_f(&self.path);
        self.file.take();
    }
}

// ---- signals ------------------------------------------------------------------------

static LOCK_C: OnceLock<CString> = OnceLock::new();
static OUT_C: OnceLock<CString> = OnceLock::new();
static LOCK_HELD: AtomicBool = AtomicBool::new(false);
static OUT_MADE: AtomicBool = AtomicBool::new(false);
static WARMUP_PID: AtomicI32 = AtomicI32::new(0);

/// A timer's stop or a Ctrl-C must not leave the lock held, a scratch file
/// behind, or the warm-up request running.
extern "C" fn on_signal(sig: libc::c_int) {
    let pid = WARMUP_PID.load(Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: kill is async-signal-safe.
        unsafe { libc::kill(-pid, libc::SIGTERM) };
    }
    for (held, path) in [(&LOCK_HELD, &LOCK_C), (&OUT_MADE, &OUT_C)] {
        if held.load(Ordering::SeqCst)
            && let Some(p) = path.get()
        {
            // SAFETY: unlink is async-signal-safe and takes a valid C string.
            unsafe { libc::unlink(p.as_ptr()) };
        }
    }
    // SAFETY: _exit is async-signal-safe.
    unsafe { libc::_exit(128 + sig) };
}

fn set_signal_handlers() {
    // For the rest of the run, so the old dispositions are not kept. A signal
    // that was ignored on entry (`nohup`) stays ignored.
    let _ = catch_signals(on_signal);
}

// ---- the log ---------------------------------------------------------------------------

struct Run {
    dry: bool,
    log: String,
}

impl Run {
    /// One decision line, several when the text has several. A dry run prints it
    /// and a real run appends it to the log, timestamped.
    fn log(&self, text: &str) {
        let ts = local_fmt(wall_secs(), "%F %T").unwrap_or_default();
        for line in text.split('\n') {
            if self.dry {
                println!("{ts} {line}");
                continue;
            }
            let path = Path::new(&self.log);
            let wrote = path
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .is_none_or(|d| std::fs::create_dir_all(d).is_ok())
                && OpenOptions::new()
                    .append(true)
                    .create(true)
                    .mode(0o666)
                    .open(path)
                    .and_then(|mut f| f.write_all(format!("{ts} {line}\n").as_bytes()))
                    .is_ok();
            if !wrote {
                eprintln!("sharkswap rotate: cannot write {}", self.log);
            }
        }
    }

    /// A fault of the run itself (not a decision): on stderr for whoever runs it by
    /// hand, and in the log for the timer, whose stderr nobody reads.
    fn error(&self, msg: &str) {
        eprintln!("sharkswap rotate: {msg}");
        if !self.dry && !self.log.is_empty() {
            self.log(&format!("ERROR: {msg}"));
        }
    }

    /// A state write, tried twice: a failed write after a real switch would leave
    /// the next run without last_switch and let it switch again inside the gap.
    fn state(&self, op: &str, write: impl Fn() -> Result<(), String>) -> bool {
        if write().is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_secs(1));
        match write() {
            Ok(()) => true,
            Err(e) => {
                let e = if e.is_empty() {
                    "no message".to_string()
                } else {
                    e
                };
                self.error(&format!("could not write the state file (state {op}): {e}"));
                false
            }
        }
    }
}

// ---- probing ------------------------------------------------------------------------------

/// What one probe run learned about one account.
#[derive(Clone, Debug)]
struct Record {
    kind: &'static str,
    reading: Option<Reading>,
    denied: bool,
    /// None for a placeholder that says nothing about the network.
    no_reply: Option<bool>,
    last_resort: bool,
}

impl Record {
    /// The network answered for that account: an HTTP response, a reading, or a
    /// refused credential. A placeholder is no answer.
    fn answered(&self) -> bool {
        self.no_reply == Some(false) || self.reading.is_some() || self.denied
    }

    fn placeholder(denied: bool, last_resort: bool) -> Record {
        Record {
            kind: "snapshot",
            reading: None,
            denied,
            no_reply: None,
            last_resort,
        }
    }

    fn from_reply(kind: &str, reply: &Reply) -> Record {
        let reading = reading_from_replies(&[reply]);
        Record {
            kind: if kind == "token" { "token" } else { "snapshot" },
            denied: reading.is_none() && denied_by_replies(&[reply]),
            no_reply: Some(no_reply_from(&[reply])),
            reading,
            last_resort: false,
        }
    }
}

/// A saved login that `sharkswap <alias>` could install but that cannot be
/// read now: its access token has lapsed, so the probe has no credential for it,
/// while its refresh token still holds. claude refreshes it on first use.
fn stored_login(ctx: &Ctx, alias: &str) -> bool {
    let f = ctx.snap(alias);
    if !f.is_file() || !valid_oauth_file(&f) {
        return false;
    }
    ctx.login_expiry_sec(&f).is_none_or(|sec| sec > wall_secs())
}

/// The stored login of `alias` as the credential to read it with, when its access
/// token is still good for more than a minute (the probe never refreshes one).
fn snapshot_cred(ctx: &Ctx, alias: &str) -> Option<Cred> {
    let f = ctx.snap(alias);
    if !f.is_file() {
        return None;
    }
    let (tok, exp) = file_login(&f)?;
    (exp > (wall_secs() + 60) as f64 * 1000.0).then_some(Cred {
        kind: "snapshot",
        tok,
    })
}

/// Read these accounts in parallel. An alias with no credential to read and none
/// to switch to has no record: it is not there for the decision. One that only
/// has a saved login too old to read gets a last-resort record. Metadata in the
/// store is required, because `sharkswap <alias>` refuses an alias without it.
/// The current account may be read through any credential, a pane home's
/// included: it is the account already in use, and what is read is its quota. A
/// CANDIDATE whose freshest credential is a pane home is not one
/// `sharkswap <alias>` installs: it is read through its stored login when that
/// login's access token is still good, and otherwise not readable.
fn probe_many(ctx: &Ctx, cur: &str, aliases: &[String]) -> BTreeMap<String, Record> {
    let mut out = BTreeMap::new();
    let mut runs: Vec<(&String, Cred)> = Vec::new();
    for a in aliases {
        if !ctx.meta(a).is_file() {
            continue;
        }
        let cred = probe_cred(ctx, a, cur).and_then(|c| {
            if c.kind != "home" || a == cur {
                Some(c)
            } else {
                snapshot_cred(ctx, a)
            }
        });
        match cred {
            Some(c) => runs.push((a, c)),
            None if stored_login(ctx, a) => {
                out.insert(a.clone(), Record::placeholder(false, true));
            }
            None => {}
        }
    }
    std::thread::scope(|s| {
        let handles: Vec<_> = runs
            .iter()
            .map(|(a, c)| (*a, c.kind, s.spawn(|| ask(&c.tok))))
            .collect();
        for (a, kind, h) in handles {
            let reply = h.join().unwrap_or_else(|_| Reply::none());
            out.insert(a.clone(), Record::from_reply(kind, &reply));
        }
    });
    out
}

/// The readings a decision wants from a list of aliases' records.
fn readings_of(
    records: &BTreeMap<String, Record>,
    aliases: &[String],
) -> (Readings, HashMap<String, String>, Vec<String>, bool) {
    let (mut out, mut kinds, mut last_resort, mut answered) =
        (Readings::new(), HashMap::new(), Vec::new(), false);
    for a in aliases {
        let Some(r) = records.get(a) else { continue };
        out.insert(a.clone(), r.reading.clone());
        kinds.insert(a.clone(), r.kind.to_string());
        if r.last_resort {
            last_resort.push(a.clone());
        }
        if r.answered() {
            answered = true;
        }
    }
    (out, kinds, last_resort, answered)
}

/// The pool file's aliases in file order: whitespace separated, `#` to the end of
/// the line is a comment, a word that cannot be an alias or is repeated is dropped.
/// A missing file is an empty pool.
fn pool_words(ctx: &Ctx, run: &Run, file: &str) -> Vec<String> {
    let Ok(text) = read_text(Path::new(file)) else {
        return Vec::new();
    };
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();
    for line in text.split('\n') {
        let line = line.split('#').next().unwrap_or("");
        for w in line.split([' ', '\t']).filter(|w| !w.is_empty()) {
            if !seen.insert(w) {
                continue;
            }
            if alias_ok(w, false) {
                // "Work" opens work's files on a case-insensitive store but is not
                // work to the ceiling table: left out, it cannot be probed, ranked
                // or switched to under a name no ceiling matches.
                if let Spelling::Variant(sp) = alias_spelling(w, &[&ctx.store, &ctx.tokens]) {
                    run.error(&format!("pool alias '{w}' is spelled '{sp}' in the store (aliases are case-sensitive) and was ignored"));
                    continue;
                }
                out.push(w.to_string());
            } else if COMMAND_WORDS.contains(&w) {
                run.error(&format!("pool alias '{w}' is reserved and was ignored"));
            }
        }
    }
    out
}

/// True when the host's live login (~/.claude/.credentials.json) is NOT
/// `alias`'s one-year token, and its refresh login has expired. The probe would
/// read `alias` through the token and see an open account, while every session on
/// the host runs on a login that can no longer refresh. The file is the live
/// login on Linux; a Mac keeps it in the Keychain, which is never read here.
fn live_login_dead(ctx: &Ctx, alias: &str, now: i64) -> bool {
    if !ctx.creds.is_file() || !ctx.token_check(alias, true) {
        return false;
    }
    let tok = read_last_json(&ctx.token_file(alias));
    let Some(tok) = crate::store::token_text(tok.as_ref()) else {
        return false;
    };
    let differs = match read_last_json(&ctx.creds) {
        Some(Value::Object(o)) => match o.get("claudeAiOauth") {
            None | Some(Value::Null) => true,
            Some(Value::Object(c)) => c.get("accessToken") != Some(&Value::String(tok.to_string())),
            Some(_) => false,
        },
        Some(Value::Null) => true,
        _ => false,
    };
    differs
        && ctx
            .login_expiry_sec(&ctx.creds)
            .is_some_and(|exp| exp >= 0 && exp <= now)
}

/// The directories a child of this run should find programs in. A timer runs
/// with systemd's PATH, which holds neither ~/.local/bin nor a Homebrew prefix.
/// Appended, so a tool already first in PATH is still the one that runs.
fn child_path(ctx: &Ctx) -> OsString {
    let cur = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&cur).collect();
    for d in [
        ctx.home.join(".local/bin"),
        "/home/linuxbrew/.linuxbrew/bin".into(),
        "/opt/homebrew/bin".into(),
        "/usr/local/bin".into(),
    ] {
        if !dirs.contains(&d) && d.is_dir() {
            dirs.push(d);
        }
    }
    std::env::join_paths(dirs).unwrap_or(cur)
}

/// Run `cmd` with no stdin and no output for at most `secs` seconds, in a process
/// group of its own: TERM, then KILL two seconds later. Whatever became of it.
fn bounded(secs: u64, mut cmd: Command) {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let Ok(mut child) = cmd.spawn() else { return };
    let pid = child.id() as libc::pid_t;
    WARMUP_PID.store(pid, Ordering::SeqCst);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = child.wait();
        let _ = tx.send(());
    });
    let kill = |sig: libc::c_int| {
        // SAFETY: signalling the process group this function created.
        if unsafe { libc::kill(-pid, sig) } != 0 {
            unsafe { libc::kill(pid, sig) };
        }
    };
    if rx.recv_timeout(Duration::from_secs(secs)).is_err() {
        kill(libc::SIGTERM);
        if rx.recv_timeout(Duration::from_secs(2)).is_err() {
            kill(libc::SIGKILL);
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
    }
    WARMUP_PID.store(0, Ordering::SeqCst);
}

// ---- the command ----------------------------------------------------------------------------------

fn usage() -> &'static str {
    "usage: sharkswap rotate [--pool FILE] [--log FILE] [--state FILE] [--min-gap SEC] [--tick SEC] [--dry-run]"
}

fn xdg(var: &str, home: &Path, fallback: &str) -> String {
    let v = env_or_empty(var);
    if v.is_empty() {
        home.join(fallback).to_string_lossy().into_owned()
    } else {
        v
    }
}

pub fn cmd_rotate(ctx: &Ctx, args: &[String]) -> Result<(), i32> {
    let cfg = xdg("XDG_CONFIG_HOME", &ctx.home, ".config");
    let state = xdg("XDG_STATE_HOME", &ctx.home, ".local/state");
    let mut pool_file = format!("{cfg}/sharkswap/pool");
    let mut log_file = format!("{state}/sharkswap-rotate.log");
    let mut state_file = format!("{state}/sharkswap-rotate.json");
    let (mut min_gap, mut tick) = ("900".to_string(), "600".to_string());
    let mut dry = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dry-run" => dry = true,
            "--pool" | "--log" | "--state" | "--min-gap" | "--tick" => {
                let Some(v) = it.next() else {
                    eprintln!("sharkswap rotate: {a} needs a value");
                    eprintln!("{}", usage());
                    return Err(2);
                };
                match a.as_str() {
                    "--pool" => pool_file = v.clone(),
                    "--log" => log_file = v.clone(),
                    "--state" => state_file = v.clone(),
                    "--min-gap" => min_gap = v.clone(),
                    _ => tick = v.clone(),
                }
            }
            other => {
                eprintln!("sharkswap rotate: unknown argument '{other}'");
                eprintln!("{}", usage());
                return Err(2);
            }
        }
    }
    let run = Run { dry, log: log_file };
    if !all_digits(&min_gap) {
        run.error(&format!("--min-gap takes whole seconds, got '{min_gap}'"));
        return Err(2);
    }
    if !all_digits(&tick) || tick.starts_with('0') {
        run.error(&format!("--tick takes whole seconds above 0, got '{tick}'"));
        return Err(2);
    }
    let min_gap: f64 = min_gap.parse().unwrap_or(f64::INFINITY);
    let tick: f64 = tick.parse().unwrap_or(f64::INFINITY);
    let state_path = PathBuf::from(&state_file);
    let child_path = child_path(ctx);

    set_signal_handlers();
    let now_secs = wall_secs();
    let now = now_secs as f64;

    // One run at a time. A dry run changes nothing and takes no lock.
    let lock_name = format!("{state_file}.lock");
    let _lock = if dry {
        None
    } else {
        match take_lock(Path::new(&lock_name)) {
            Lock::Held(l) => Some(l),
            Lock::Busy => {
                run.log(&format!("another run holds {lock_name}; skipping this one"));
                return Ok(());
            }
            // Without the lock a manual run and the timer could both switch, so
            // the run is skipped and the fault is logged. Unlike a busy lock this
            // is a failure: the exit status is 75 (EX_TEMPFAIL), so a timer unit
            // or a script sees that nothing ran and can try again.
            Lock::Unavailable(why) => {
                run.error(&format!(
                    "cannot take the lock {lock_name}: {why}; skipping this run"
                ));
                return Err(75);
            }
        }
    };

    let pool = pool_words(ctx, &run, &pool_file);
    if File::open(&pool_file).is_err() {
        run.error(&format!(
            "pool file {pool_file} not found; the pool is empty"
        ));
    } else if pool.is_empty() {
        run.error(&format!(
            "pool file {pool_file} has no aliases; the pool is empty"
        ));
    }

    // The current account first: one probe of one account. A login this store does
    // not know is still read from the live credentials file, so an open account is
    // never left for that. No login at all is a record of its own, a refusal:
    // there is nothing to wait for, and a host without one is better off on any
    // account than on none.
    let cur = ctx.current_alias();
    let mut cur_rec: Option<Record> = None;
    if !cur.is_empty() {
        if live_login_dead(ctx, &cur, now_secs) {
            run.log(&format!(
                "note: {cur}: the host's login is not its one-year token and its refresh login has expired; counting the account as unreadable"
            ));
            cur_rec = Some(Record::placeholder(false, false));
        } else {
            cur_rec = probe_many(ctx, &cur, std::slice::from_ref(&cur)).remove(&cur);
        }
    } else {
        let floor = (now_secs + 60) as f64 * 1000.0;
        let live = if ctx.creds.is_file() {
            file_login(&ctx.creds).filter(|(_, exp)| *exp > floor)
        } else {
            None
        };
        if let Some((tok, _)) = live {
            cur_rec = Some(Record::from_reply("active", &ask(&tok)));
        } else if ctx.current_email().is_empty() {
            cur_rec = Some(Record::placeholder(true, false));
        }
    }

    let eff = effective_caps(&ctx.store);
    let cur_key: Option<String> = alias_re_ok(&cur).then(|| cur.clone());
    let st = state_map(&state_path);
    let mut input = Input {
        current: cur_key.clone(),
        reading: cur_rec.as_ref().and_then(|r| r.reading.clone()),
        last: parse_last(st.get("last_reading")),
        pool: pool.iter().filter(|a| alias_re_ok(a)).cloned().collect(),
        caps: eff.caps.clone(),
        now,
        last_switch: finite(st.get("last_switch")).unwrap_or(0.0),
        min_gap,
        tick,
        host_rate: parse_host_rate(st.get("host_rate")),
        unreadable: match st.get("unreadable").and_then(Value::as_object) {
            Some(m)
                if m.get("alias").and_then(Value::as_str)
                    == Some(cur_key.as_deref().unwrap_or(UNKNOWN)) =>
            {
                finite(m.get("n")).unwrap_or(0.0)
            }
            _ => 0.0,
        },
        denied: cur_rec.as_ref().is_some_and(|r| r.denied),
        no_reply: !cur_rec.as_ref().is_some_and(Record::answered),
        failed: parse_failed(st.get("failed")),
        last_failed: parse_last_failed(st.get("last_failed")),
        ..Input::default()
    };

    // Then, only while the decision asks for more: the pool, and for a capped
    // account with an empty pool, the other token accounts.
    let mut decision = None;
    if eff.unusable.is_none() {
        let mut d = match decide(&input) {
            Ok(d) => d,
            Err(e) => {
                run.error(&format!("the decision failed: rotate: {e}"));
                return Err(1);
            }
        };
        for _ in 0..2 {
            if d.action != Action::Need {
                break;
            }
            if d.need == Some("pool") {
                let want: Vec<String> = pool.iter().filter(|a| **a != cur).cloned().collect();
                let pool_records = probe_many(ctx, &cur, &want);
                let (readings, kinds, last_resort, answered) =
                    readings_of(&pool_records, &input.pool);
                input.candidates = Some(readings);
                input.kinds = kinds;
                input.last_resort = last_resort;
                input.answered_candidates = answered;
            } else {
                let mut tokens: Vec<String> = Vec::new();
                for f in glob_json(&ctx.tokens) {
                    let a = &f[..f.len() - 5];
                    if !alias_ok(a, false) {
                        // A token file named like a command can never be switched to,
                        // so it is skipped; said in the log, as the pool's reserved
                        // words are.
                        if COMMAND_WORDS.contains(&a) {
                            run.error(&format!(
                                "token '{a}' is reserved and was ignored ({}/{a}.json)",
                                ctx.tokens.display()
                            ));
                        }
                        continue;
                    }
                    if a == cur || pool.iter().any(|p| p == a) || !ctx.token_check(a, true) {
                        continue;
                    }
                    tokens.push(a.to_string());
                }
                let token_records = probe_many(ctx, &cur, &tokens);
                let words: Vec<String> =
                    tokens.iter().filter(|a| alias_re_ok(a)).cloned().collect();
                let (readings, kinds, _, _) = readings_of(&token_records, &words);
                input.tokens = Some(readings);
                input.kinds.extend(kinds);
            }
            d = match decide(&input) {
                Ok(d) => d,
                Err(e) => {
                    run.error(&format!("the decision failed: rotate: {e}"));
                    return Err(1);
                }
            };
        }
        if d.action == Action::Need {
            run.error("the decision still needs readings");
            return Err(1);
        }
        decision = Some(d);
    }

    // The caps table's notes (a table that cannot be read refuses every account,
    // so it is said in the log too, not only where nobody looks).
    for note in &eff.notes {
        for line in note.split('\n') {
            eprintln!("sharkswap rotate: note: {line}");
            if !dry {
                run.log(&format!("note: {line}"));
            }
        }
    }

    // No usable ceiling policy: no account may be chosen. Nothing is switched and
    // the state file is left alone; the log gets an ERROR line.
    let Some(decision) = decision else {
        let u = eff.unusable.as_ref();
        run.error(&format!(
            "the ceiling policy {} {}; no account is chosen and no switch is made",
            u.map_or("", |u| u.file.as_str()),
            u.map_or("", |u| u.why.as_str())
        ));
        return Err(1);
    };

    // What this run saw of the current account is what the next run's rate is
    // measured from, and a run that saw nothing is one more unreadable run for it
    // (the decision leaves an unreadable account after two). A dry run leaves the
    // state alone: it changes nothing.
    if !dry {
        let key = cur_key.as_deref().unwrap_or(UNKNOWN);
        run.state("remember", || {
            remember(
                &state_path,
                key,
                input.reading.as_ref(),
                now,
                tick,
                &eff.caps,
            )
        });
    }

    match decision.action {
        Action::Stay => {
            // Silent in a real run: the timer would otherwise log every ten minutes.
            if dry {
                run.log(&decision.reason);
            }
            return Ok(());
        }
        Action::Wait => {
            run.log(&decision.reason);
            return Ok(());
        }
        _ => {}
    }

    run.log(&decision.reason);
    if dry {
        return Ok(());
    }
    let target = decision.target.unwrap_or_default();
    let kind = decision.kind.unwrap_or_default();
    // --force below lifts the ceiling refusal, so it must never carry a name the
    // ceiling table does not recognise: a target spelled differently from the
    // store's file ("Work" for work) is refused here, not switched to.
    if let Spelling::Variant(spelled) = alias_spelling(&target, &[&ctx.store, &ctx.tokens]) {
        run.log(&format!("  refusing target '{target}': the store spells it '{spelled}' and aliases are case-sensitive"));
        run.state("failed", || {
            mark(&state_path, false, &target, wall_secs() as f64, tick)
        });
        return Err(1);
    }
    // `sharkswap <alias>` in a process of its own, as an operator runs it, so
    // its own error handling stands. ASSUME_YES because nobody is at a terminal,
    // and the answer is read back below rather than trusted. --force because the
    // ceiling was already judged, above, from the readings this run took a moment
    // ago; `sharkswap <alias>` alone would judge it again from
    // availability.json, which this run does not write.
    let tmpdir = match env_or_empty("TMPDIR") {
        t if t.is_empty() => "/tmp".to_string(),
        t => t,
    };
    let out_path = match mktemp(format!("{tmpdir}/cs-rotate.")) {
        Ok(p) => p,
        Err(_) => {
            run.error(&format!("cannot make a work directory under {tmpdir}"));
            return Err(1);
        }
    };
    if let Ok(c) = CString::new(out_path.as_os_str().as_bytes()) {
        let _ = OUT_C.set(c);
        OUT_MADE.store(true, Ordering::SeqCst);
    }
    let spawned = File::options().append(true).open(&out_path).and_then(|f| {
        let g = f.try_clone()?;
        let exe = std::env::current_exe()?;
        Command::new(exe)
            .arg(&target)
            .arg("--force")
            .env("ASSUME_YES", "1")
            .env("PATH", &child_path)
            .stdout(Stdio::from(f))
            .stderr(Stdio::from(g))
            .status()
    });
    let rc = spawned.map_or(127, |s| s.code().unwrap_or(128));
    let out_text = read_text(&out_path).unwrap_or_default();
    rm_f(&out_path);
    OUT_MADE.store(false, Ordering::SeqCst);

    let now_alias = ctx.current_alias();
    // The host moved when the login is not the one it started on, whether or not
    // it landed on the target: a half-done swap still counts as a switch for the
    // gap, or the next run would try again at once. Recorded before anything slow.
    let (mut moved, mut recorded) = (false, false);
    if now_alias != cur {
        moved = true;
        let who = if now_alias.is_empty() {
            UNKNOWN
        } else {
            now_alias.as_str()
        };
        recorded = run.state("switched", || {
            mark(&state_path, true, who, wall_secs() as f64, tick)
        });
    }
    if now_alias == target {
        run.log(&format!(
            "  switch to {target} rc={rc}; now on {now_alias}; verified"
        ));
    } else {
        run.log(&format!(
            "  switch to {target} rc={rc}; now on {}; NOT verified",
            if now_alias.is_empty() {
                "?"
            } else {
                &now_alias
            }
        ));
        for line in out_text
            .split('\n')
            .filter(|l| l.starts_with("error"))
            .take(3)
        {
            run.log(&format!("  {line}"));
        }
        if moved {
            run.log(&format!(
                "  the host moved (from {} to {}) though not onto {target}; the switch gap counts from now",
                if cur.is_empty() { "none" } else { &cur },
                if now_alias.is_empty() { "none" } else { &now_alias }
            ));
        }
        run.state("failed", || {
            mark(&state_path, false, &target, wall_secs() as f64, tick)
        });
        return Err(1);
    }
    if kind == "snapshot" {
        // A saved login carries a short-lived access token, and a lapsed one only
        // comes back when claude uses it. One tiny request now, so the login is
        // refreshed by this run and not by the operator's first prompt. Bounded
        // (ROTATE_WARMUP_SECS, default 120) and with no stdin.
        let secs = env_or_empty("ROTATE_WARMUP_SECS");
        let secs: u64 = if all_digits(&secs) && !secs.starts_with('0') {
            secs.parse().unwrap_or(120)
        } else {
            120
        };
        let mut cmd = Command::new("claude");
        cmd.args(["-p", "reply OK", "--model", &probe_model()])
            .env("PATH", &child_path);
        bounded(secs, cmd);
    }
    // A verified switch whose record was lost is still a failure of this run: the
    // next one would see no last_switch and may switch again inside the gap.
    if moved && !recorded {
        return Err(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::Ceiling;
    use crate::testkit::NOW;
    use serde_json::json;

    const T0: f64 = NOW / 1000.0;

    fn open(five: f64, week: f64) -> Reading {
        Reading {
            five: Some(five),
            week: Some(week),
            five_status: "allowed".into(),
            week_status: "allowed".into(),
        }
    }

    fn spent() -> Reading {
        Reading {
            five: Some(1.0),
            week: Some(0.4),
            five_status: "rejected".into(),
            week_status: "allowed".into(),
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn readings(list: &[(&str, Option<Reading>)]) -> Option<Readings> {
        Some(
            list.iter()
                .map(|(a, r)| (a.to_string(), r.clone()))
                .collect(),
        )
    }

    fn five_cap(alias: &str, pct: f64) -> Caps {
        Caps(vec![(
            alias.to_string(),
            Ceiling {
                h5: Some(pct),
                d7: None,
            },
        )])
    }

    /// The host is on "work" with this reading, at `T0`.
    fn on_work(reading: Option<Reading>) -> Input {
        Input {
            current: Some("work".into()),
            reading,
            now: T0,
            ..Input::default()
        }
    }

    #[test]
    fn decide_an_open_account_stays() {
        let d = decide(&Input {
            pool: names(&["spare"]),
            ..on_work(Some(open(0.3, 0.2)))
        })
        .unwrap();
        assert_eq!(d.action, Action::Stay);
    }

    #[test]
    fn decide_uses_an_account_until_it_is_refused_however_high_the_figure() {
        let d = decide(&Input {
            pool: names(&["spare"]),
            ..on_work(Some(open(0.97, 0.97)))
        })
        .unwrap();
        assert_eq!(d.action, Action::Stay);
    }

    #[test]
    fn decide_a_refused_account_asks_for_the_pool_then_moves_to_the_one_with_most_room() {
        let input = Input {
            pool: names(&["a", "b", "work"]),
            last_switch: 0.0,
            ..on_work(Some(spent()))
        };
        assert_eq!(decide(&input).unwrap().need, Some("pool"));
        let input = Input {
            candidates: readings(&[
                ("a", Some(open(0.5, 0.1))),
                ("b", Some(open(0.2, 0.3))),
                ("work", Some(spent())),
            ]),
            ..input
        };
        let d = decide(&input).unwrap();
        assert_eq!(d.action, Action::Switch);
        assert_eq!(
            d.target.as_deref(),
            Some("b"),
            "b's tighter window (0.3) beats a's (0.5)"
        );
        assert_eq!(d.kind.as_deref(), Some("token"));
        assert!(d.reason.starts_with("switch: work ("), "{}", d.reason);
        assert!(d.reason.contains(") -> b ["), "{}", d.reason);
    }

    #[test]
    fn decide_ties_on_the_tighter_window_go_to_the_lower_5h_then_pool_order() {
        let input = Input {
            pool: names(&["a", "b", "c"]),
            candidates: readings(&[
                ("a", Some(open(0.4, 0.4))),
                ("b", Some(open(0.1, 0.4))),
                ("c", Some(open(0.1, 0.4))),
            ]),
            last_switch: 0.0,
            ..on_work(Some(spent()))
        };
        assert_eq!(decide(&input).unwrap().target.as_deref(), Some("b"));
    }

    #[test]
    fn decide_passes_on_the_kind_of_credential_behind_the_pick() {
        let input = Input {
            pool: names(&["a"]),
            candidates: readings(&[("a", Some(open(0.1, 0.1)))]),
            kinds: HashMap::from([("a".to_string(), "snapshot".to_string())]),
            ..on_work(Some(spent()))
        };
        assert_eq!(decide(&input).unwrap().kind.as_deref(), Some("snapshot"));
    }

    #[test]
    fn decide_the_minimum_gap_holds_a_refused_account_in_place() {
        let input = Input {
            pool: names(&["a"]),
            candidates: readings(&[("a", Some(open(0.1, 0.1)))]),
            last_switch: T0 - 60.0,
            min_gap: DEFAULT_MIN_GAP,
            ..on_work(Some(spent()))
        };
        let d = decide(&input).unwrap();
        assert_eq!(d.action, Action::Wait);
        assert!(d.reason.contains("last switch was under"), "{}", d.reason);
        let later = decide(&Input {
            last_switch: T0 - DEFAULT_MIN_GAP - 1.0,
            ..input
        })
        .unwrap();
        assert_eq!(later.action, Action::Switch);
    }

    #[test]
    fn decide_nothing_open_in_the_pool_means_wait() {
        let input = Input {
            pool: names(&["a"]),
            candidates: readings(&[("a", Some(spent()))]),
            ..on_work(Some(spent()))
        };
        let d = decide(&input).unwrap();
        assert_eq!(d.action, Action::Wait);
        assert!(d.reason.contains("no pool account open"), "{}", d.reason);
    }

    #[test]
    fn decide_an_account_at_its_ceiling_leaves_at_once_gap_or_not() {
        let input = Input {
            pool: names(&["a"]),
            candidates: readings(&[("a", Some(open(0.1, 0.1)))]),
            caps: five_cap("work", 80.0),
            last_switch: T0 - 10.0,
            ..on_work(Some(open(0.85, 0.1)))
        };
        let d = decide(&input).unwrap();
        assert_eq!(d.action, Action::Switch);
        assert!(d.reason.contains("at its ceiling"), "{}", d.reason);
    }

    #[test]
    fn decide_the_burn_rate_projects_an_account_over_its_ceiling_before_the_next_tick() {
        let input = Input {
            pool: names(&["a"]),
            candidates: readings(&[("a", Some(open(0.1, 0.1)))]),
            caps: five_cap("work", 90.0),
            tick: 600.0,
            ..on_work(Some(open(0.85, 0.1)))
        };
        assert_eq!(
            decide(&input).unwrap().action,
            Action::Stay,
            "85% against a 90% ceiling, no rate known"
        );
        let last = Last {
            alias: Some("work".into()),
            t: Some(T0 - 300.0),
            five: Some(0.75),
            week: Some(0.1),
        };
        let fast = decide(&Input {
            last: Some(last),
            ..input
        })
        .unwrap();
        assert_eq!(
            fast.action,
            Action::Switch,
            "+10 points in 5 minutes would pass 90% within a tick"
        );
    }

    #[test]
    fn decide_skips_a_candidate_that_cannot_be_shown_to_be_under_its_ceiling() {
        let blind = Reading {
            five: None,
            week: Some(0.1),
            five_status: String::new(),
            week_status: String::new(),
        };
        let input = Input {
            pool: names(&["capped", "plain"]),
            candidates: readings(&[("capped", Some(blind)), ("plain", Some(open(0.6, 0.6)))]),
            caps: five_cap("capped", 80.0),
            ..on_work(Some(spent()))
        };
        assert_eq!(decide(&input).unwrap().target.as_deref(), Some("plain"));
    }

    #[test]
    fn decide_a_capped_account_with_an_empty_pool_may_use_a_token_account_outside_it() {
        let input = Input {
            pool: Vec::new(),
            candidates: readings(&[]),
            caps: five_cap("work", 80.0),
            ..on_work(Some(open(0.9, 0.1)))
        };
        assert_eq!(decide(&input).unwrap().need, Some("tokens"));
        let d = decide(&Input {
            tokens: readings(&[("solo", Some(open(0.1, 0.1)))]),
            ..input
        })
        .unwrap();
        assert_eq!(d.action, Action::Switch);
        assert_eq!(d.target.as_deref(), Some("solo"));
        assert!(d.reason.contains("leaving the pool"), "{}", d.reason);
    }

    #[test]
    fn decide_a_refused_but_uncapped_account_waits_for_the_pool_and_does_not_go_outside_it() {
        let input = Input {
            pool: Vec::new(),
            candidates: readings(&[]),
            tokens: readings(&[("solo", Some(open(0.1, 0.1)))]),
            ..on_work(Some(spent()))
        };
        assert_eq!(decide(&input).unwrap().action, Action::Wait);
    }

    #[test]
    fn decide_an_unreadable_account_is_kept_for_one_run_left_on_the_second_and_a_refused_credential_at_once()
     {
        let base = Input {
            pool: names(&["a"]),
            candidates: readings(&[("a", Some(open(0.1, 0.1)))]),
            ..on_work(None)
        };
        let first = decide(&base).unwrap();
        assert_eq!(first.action, Action::Wait);
        assert!(first.reason.contains("run 1 of 2"), "{}", first.reason);
        assert_eq!(
            decide(&Input {
                unreadable: 1.0,
                ..base.clone()
            })
            .unwrap()
            .action,
            Action::Switch
        );
        assert_eq!(
            decide(&Input {
                denied: true,
                ..base
            })
            .unwrap()
            .action,
            Action::Switch
        );
    }

    #[test]
    fn decide_a_target_whose_switch_just_failed_is_ranked_last_and_used_if_it_is_all_there_is() {
        let base = Input {
            pool: names(&["a", "b"]),
            failed: vec![("a".into(), T0 - 100.0)],
            ..on_work(Some(spent()))
        };
        let both = readings(&[("a", Some(open(0.1, 0.1))), ("b", Some(open(0.5, 0.5)))]);
        let d = decide(&Input {
            candidates: both.clone(),
            ..base.clone()
        })
        .unwrap();
        assert_eq!(d.target.as_deref(), Some("b"));
        let only = decide(&Input {
            pool: names(&["a"]),
            candidates: readings(&[("a", Some(open(0.1, 0.1)))]),
            ..base.clone()
        })
        .unwrap();
        assert_eq!(only.target.as_deref(), Some("a"));
        assert!(only.reason.contains("ranked last"), "{}", only.reason);
        let old = decide(&Input {
            failed: vec![("a".into(), T0 - 100_000.0)],
            candidates: both,
            ..base
        })
        .unwrap();
        assert_eq!(
            old.target.as_deref(),
            Some("a"),
            "an old failure is forgotten"
        );
    }

    #[test]
    fn decide_a_saved_login_that_cannot_be_probed_is_a_last_resort_and_only_when_something_answered()
     {
        let base = Input {
            pool: names(&["stored"]),
            candidates: readings(&[("stored", None)]),
            last_resort: names(&["stored"]),
            ..on_work(Some(spent()))
        };
        let d = decide(&base).unwrap();
        assert_eq!(d.action, Action::Switch);
        assert_eq!(d.kind.as_deref(), Some("snapshot"));
        let quiet = decide(&Input {
            no_reply: true,
            answered_candidates: false,
            ..base
        })
        .unwrap();
        assert_eq!(quiet.action, Action::Wait);
    }

    #[test]
    fn decide_now_must_be_a_number() {
        let input = Input {
            current: Some("work".into()),
            ..Input::default()
        };
        assert!(decide(&input).is_err());
    }

    #[test]
    fn refused_and_blocked() {
        assert!(refused(&spent()));
        assert!(!refused(&open(0.99, 0.99)));
        let caps = five_cap("w", 80.0);
        assert!(blocked(&open(0.85, 0.0), "w", &caps, None, DEFAULT_TICK));
        assert!(!blocked(&open(0.5, 0.0), "w", &caps, None, DEFAULT_TICK));
    }

    #[test]
    fn rate_since_only_the_same_account_only_a_rise_only_when_fresh() {
        let last = Last {
            alias: Some("work".into()),
            t: Some(1000.0),
            five: Some(0.2),
            week: Some(0.5),
        };
        let now_reading = Reading {
            five: Some(0.4),
            week: Some(0.5),
            ..Reading::default()
        };
        let rise = rate_since(Some(&last), "work", Some(&now_reading), 1200.0, 600.0);
        assert_eq!(
            rise,
            Some(Rate {
                five: Some(0.001),
                week: None
            })
        );
        assert_eq!(
            rate_since(Some(&last), "other", Some(&now_reading), 1200.0, 600.0),
            None
        );
        assert_eq!(
            rate_since(
                Some(&last),
                "work",
                Some(&now_reading),
                1000.0 + 4.0 * 600.0,
                600.0
            ),
            None,
            "older than three ticks"
        );
        let fell = Reading {
            five: Some(0.1),
            week: Some(0.1),
            ..Reading::default()
        };
        assert_eq!(
            rate_since(Some(&last), "work", Some(&fell), 1200.0, 600.0),
            Some(Rate::default()),
            "a fall means the window reset"
        );
    }

    #[test]
    fn recent_failures_forgets_old_and_future_entries() {
        let failed = [
            ("a".to_string(), 9900.0),
            ("b".to_string(), 1000.0),
            ("c".to_string(), 20000.0),
        ];
        let from_old_format = ("d".to_string(), 9500.0);
        let m = recent_failures(&failed, Some(&from_old_format), 10000.0, 600.0);
        let mut aliases: Vec<&str> = m.iter().map(|(a, _)| a.as_str()).collect();
        aliases.sort_unstable();
        assert_eq!(aliases, ["a", "d"]);
    }

    fn reply(http: u16, limits: serde_json::Value) -> Reply {
        Reply {
            http,
            limits: limits.as_object().cloned().unwrap_or_default(),
        }
    }

    #[test]
    fn reading_from_replies_and_denied_by_replies() {
        let ok = reply(
            200,
            json!({ "5h": { "util": 0.4, "status": "allowed" }, "7d": { "util": 0.5, "status": "allowed" } }),
        );
        assert_eq!(reading_from_replies(&[&ok]), Some(open(0.4, 0.5)));
        assert_eq!(reading_from_replies(&[&reply(401, json!({}))]), None);
        assert_eq!(reading_from_replies(&[&reply(200, json!({}))]), None);
        let first = reading_from_replies(&[&Reply::none(), &ok]).unwrap();
        assert_eq!(
            first.five,
            Some(0.4),
            "the first reply that has the figure supplies it"
        );
        assert!(denied_by_replies(&[&reply(403, json!({}))]));
        assert!(!denied_by_replies(&[&reply(429, json!({}))]));
    }

    #[test]
    fn js_str_prints_numbers_the_way_javascript_does() {
        assert_eq!(js_str(600.0), "600");
        assert_eq!(js_str(0.5), "0.5");
        assert_eq!(js_str(0.001), "0.001");
        assert_eq!(js_str(-2.25), "-2.25");
        assert_eq!(js_str(1.5e-7), "1.5e-7");
        assert_eq!(js_str(1e21), "1e+21");
        assert_eq!(js_str(123_456_789_012.0), "123456789012");
        assert_eq!(js_str(0.0), "0");
        assert_eq!(js_str(f64::NAN), "NaN");
        assert_eq!(js_str(f64::NEG_INFINITY), "-Infinity");
    }
}
