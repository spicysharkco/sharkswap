//! Per-account usage ceilings: an account you want to keep some headroom on
//! (a shared one, a work account, one you only mean to spend part of) stops
//! being used once it reaches a set percentage of its 5-hour or its weekly
//! window, well before the provider would refuse it. Every command that hands
//! out or shows a launch target agrees about it through this one module:
//! `best`, `bar`, the rotator, and the refusal `cs <alias>` and `cs pane` give
//! (`gate`).
//!
//! The table is `$STORE/account-caps.json`:
//!
//! ```text
//! {"caps": {"<alias>": {"5h": 80, "7d": 90}}}
//! ```
//!
//! a ceiling being a percent from 1 to 100, for either window or both. No file
//! means no ceilings.
//!
//! One spelling: a ceiling key is lowercase, and a key in another case is read
//! as its lowercase alias, never ignored. Every lookup lowercases the alias it
//! is given (`ceiling_of`).
//!
//! One failure rule: a table that exists but cannot be used at all (empty, not
//! JSON, truncated, or holding no `caps` object) leaves the policy UNUSABLE,
//! and then NO account may be launched or ranked: a reader that cannot read the
//! promise must not guess which accounts it covers. A single broken entry is
//! milder: that alias stays capped and is always refused (`BROKEN`), the rest of
//! the table stands.
//!
//! One function answers "which aliases have which ceilings": `effective_caps`.
//!
//! The figures are the windows `cs probe` read off the rate-limit headers.
//! Headers can read a few points above the usage page; the percentages are
//! compared as recorded, so a 95% ceiling stops an account at or before the
//! page's 95%.

use crate::avail::{
    Avail, Emails, Window, Windows, alias_emails, known_aliases, read_availability, windows_for,
};
use crate::util::{js_round, read_text};
use serde_json::Value;
use std::path::Path;

/// Ceiling name to window key, week first: when both are over, the longer wait
/// is the one worth naming.
const CAP_WINDOWS: [&str; 2] = ["7d", "5h"];

/// The ceiling a broken entry is read as: 0% of either window, which every
/// reading is at or over and which no reading at all counts as over too. A
/// valid ceiling is at least 1. Fail closed: a typo must tighten the gate.
pub const BROKEN: f64 = 0.0;

const NOT_ALIASES: [&str; 3] = ["__proto__", "constructor", "prototype"];

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Ceiling {
    pub h5: Option<f64>,
    pub d7: Option<f64>,
}

impl Ceiling {
    pub fn get(&self, name: &str) -> Option<f64> {
        match name {
            "5h" => self.h5,
            "7d" => self.d7,
            _ => None,
        }
    }
    fn set(&mut self, name: &str, v: f64) {
        match name {
            "5h" => self.h5 = Some(v),
            _ => self.d7 = Some(v),
        }
    }
    /// Per-window lower figure.
    fn lower(&mut self, other: &Ceiling) {
        for name in ["5h", "7d"] {
            if let Some(v) = other.get(name)
                && !self.get(name).is_some_and(|mine| mine <= v)
            {
                self.set(name, v);
            }
        }
    }
    pub fn has_broken(&self) -> bool {
        self.h5 == Some(BROKEN) || self.d7 == Some(BROKEN)
    }
}

/// Alias to ceiling, in insertion order.
#[derive(Clone, Debug, Default)]
pub struct Caps(pub Vec<(String, Ceiling)>);

impl Caps {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn get(&self, key: &str) -> Option<&Ceiling> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, c)| c)
    }
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_str())
    }
    fn put(&mut self, key: &str, c: Ceiling) {
        match self.0.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = c,
            None => self.0.push((key.to_string(), c)),
        }
    }
}

/// A value out of a file, bounded before it reaches a terminal.
fn shown(v: &Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_else(|_| v.to_string());
    if s.chars().count() > 40 {
        let head: String = s.chars().take(37).collect();
        format!("{head}...")
    } else {
        s
    }
}

fn alias_ok(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

enum Validated {
    NotTable(&'static str),
    Table { caps: Caps, notes: Vec<String> },
}

/// The table as the file spells it, validated. An invalid entry keeps its alias
/// capped and always refused (`BROKEN`). A key that is not an alias at all caps
/// nothing, so it is only noted. Entries that meet at the lowercase alias
/// merge, the lower figure per window winning, so `BROKEN` wins.
fn validate_caps(raw: &Value) -> Validated {
    let Some(table) = raw.get("caps").and_then(Value::as_object) else {
        return Validated::NotTable("has no \"caps\" object");
    };
    let mut caps = Caps::default();
    let mut notes = Vec::new();
    let tail = " \u{2014} alias kept capped and refused";
    for (spelled, windows) in table {
        let alias = spelled.to_lowercase();
        if !alias_ok(spelled) || alias.starts_with('.') || NOT_ALIASES.contains(&alias.as_str()) {
            notes.push(format!(
                "{} is not an alias \u{2014} ignored",
                shown(&Value::String(spelled.clone()))
            ));
            continue;
        }
        let mut mine = Ceiling::default();
        let mut bad: Vec<String> = Vec::new();
        match windows.as_object() {
            None => bad.push(format!(
                "{spelled}: {} is not {{\"5h\": n, \"7d\": n}}{tail}",
                shown(windows)
            )),
            Some(obj) => {
                for (name, v) in obj {
                    if name != "5h" && name != "7d" {
                        bad.push(format!(
                            "{spelled}: {} is not a window (5h or 7d){tail}",
                            shown(&Value::String(name.clone()))
                        ));
                        continue;
                    }
                    match v
                        .as_f64()
                        .filter(|n| n.is_finite() && *n >= 1.0 && *n <= 100.0)
                    {
                        Some(n) => mine.set(name, n),
                        None => bad.push(format!(
                            "{spelled} {name}: {} is not a percent from 1 to 100{tail}",
                            shown(v)
                        )),
                    }
                }
                if bad.is_empty() && mine.h5.is_none() && mine.d7.is_none() {
                    bad.push(format!("{spelled}: has no 5h or 7d ceiling{tail}"));
                }
            }
        }
        let broken = !bad.is_empty();
        notes.extend(bad);
        let entry = if broken {
            Ceiling {
                h5: Some(BROKEN),
                d7: Some(BROKEN),
            }
        } else {
            mine
        };
        match caps.0.iter_mut().find(|(k, _)| *k == alias) {
            None => caps.0.push((alias, entry)),
            Some((_, had)) => had.lower(&entry),
        }
    }
    Validated::Table { caps, notes }
}

#[derive(Clone, Debug)]
pub struct Unusable {
    pub file: String,
    pub why: String,
}

#[derive(Clone, Debug, Default)]
pub struct ReadCaps {
    pub caps: Caps,
    pub path: Option<String>,
    pub notes: Vec<String>,
    pub unusable: Option<Unusable>,
}

/// The table in force. `unusable` is None when a table applies (or none
/// exists); when set, `caps` is empty and every caller refuses to launch or
/// pick ANY account.
pub fn read_caps(store: &Path) -> ReadCaps {
    let file_path = store.join("account-caps.json");
    let file = file_path.to_string_lossy().into_owned();
    let text = match read_text(&file_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ReadCaps::default(),
        Err(_) => {
            let warning = format!("WARNING: {file} is unreadable");
            return ReadCaps {
                notes: vec![warning],
                unusable: Some(Unusable {
                    file,
                    why: "is unreadable".into(),
                }),
                ..ReadCaps::default()
            };
        }
    };
    let why = match serde_json::from_str::<Value>(&text) {
        Err(_) => "is not valid JSON",
        Ok(raw) => match validate_caps(&raw) {
            Validated::Table { caps, notes } => {
                let notes: Vec<String> = notes.iter().map(|n| format!("{file}: {n}")).collect();
                return ReadCaps {
                    caps,
                    path: Some(file),
                    notes,
                    unusable: None,
                };
            }
            Validated::NotTable(why) => why,
        },
    };
    let warning = format!(
        "WARNING: {file} {why} \u{2014} the account ceiling policy is UNUSABLE, so every account is refused until the file is fixed or removed (a launch needs --force)"
    );
    ReadCaps {
        notes: vec![warning],
        unusable: Some(Unusable {
            file,
            why: why.into(),
        }),
        ..ReadCaps::default()
    }
}

/// The ceiling table's entry for one alias, or None. The alias is lowercased
/// first: the store may be on macOS (case-insensitive: `cs Work` opens work's
/// files), and a lookup by the name as typed would let "Work" through with no
/// ceiling at all. Keys that differ only in case merge, the lower figure per
/// window winning.
pub fn ceiling_of(caps: &Caps, alias: &str) -> Option<Ceiling> {
    let want = alias.to_lowercase();
    let mut out: Option<Ceiling> = None;
    for (key, c) in &caps.0 {
        if key.to_lowercase() != want {
            continue;
        }
        out.get_or_insert_with(Ceiling::default).lower(c);
    }
    out
}

/// How long a reading with no reset time describes its window: as long as the
/// window is.
fn window_ms(name: &str) -> f64 {
    match name {
        "5h" => 5.0 * 3_600_000.0,
        "7d" => 7.0 * 86_400_000.0,
        _ => 0.0,
    }
}

/// Whether one window still describes the period it names at `now_ms`:
/// observed at a real instant not ahead of the clock, and not past its reset. A
/// window that names no reset counts for its own length from the observation,
/// and never beyond `horizon_ms`, the probe horizon the caller holds its
/// readings to.
pub fn current(w: Option<&Window>, now_ms: f64, name: &str, horizon_ms: f64) -> bool {
    let Some(w) = w else { return false };
    if !w.observed_ms.is_finite() || w.observed_ms > now_ms {
        return false;
    }
    if let Some(r) = w.reset_ms {
        return r > now_ms;
    }
    now_ms - w.observed_ms < window_ms(name).min(horizon_ms)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub window: &'static str,
    pub used_pct: f64,
    pub cap: f64,
    pub reset_ms: Option<f64>,
}

/// Which ceiling one account is at or over, from its windows. `used_pct` is
/// rounded the way every display rounds it, so an account shown at "95%" is at
/// a 95% ceiling. The week is named when both are over.
pub fn over_cap(
    alias: &str,
    windows: &Windows,
    caps: &Caps,
    now_ms: f64,
    horizon_ms: f64,
) -> Option<Hit> {
    let ceiling = ceiling_of(caps, alias)?;
    for name in CAP_WINDOWS {
        let Some(cap) = ceiling.get(name) else {
            continue;
        };
        let Some(w) = windows.get(name) else { continue };
        if !current(Some(w), now_ms, name, horizon_ms) {
            continue;
        }
        let used = js_round(w.used_pct).max(0.0);
        if used.is_finite() && used >= cap {
            return Some(Hit {
                window: name,
                used_pct: used,
                cap,
                reset_ms: w.reset_ms,
            });
        }
    }
    None
}

/// "5h 96% (ceiling 95%)": the words every view uses for one hit.
pub fn cap_label(hit: &Hit) -> String {
    if hit.cap == BROKEN {
        format!(
            "{} (its ceiling entry is invalid, so it counts as over)",
            hit.window
        )
    } else {
        format!("{} {}% (ceiling {}%)", hit.window, hit.used_pct, hit.cap)
    }
}

/// The table plus every store alias it covers that it does not name as typed:
/// one that matches an entry up to case, and one saved for the same login (the
/// same email) as a capped alias, whatever it is called. Ceilings of aliases
/// sharing a login merge, the lower figure per window winning.
fn widen(table: &Caps, emails: &Emails, names: &[String]) -> Caps {
    let mut by_email: Vec<(String, Ceiling)> = Vec::new();
    for alias in names {
        let Some(email) = emails.get(alias) else {
            continue;
        };
        let Some(c) = ceiling_of(table, alias) else {
            continue;
        };
        let email = email.to_lowercase();
        match by_email.iter_mut().find(|(e, _)| *e == email) {
            Some((_, held)) => held.lower(&c),
            None => by_email.push((email, c)),
        }
    }
    let mut out = table.clone();
    for alias in names {
        if NOT_ALIASES.contains(&alias.as_str()) {
            continue;
        }
        let mut merged = ceiling_of(table, alias);
        if let Some(email) = emails.get(alias) {
            let email = email.to_lowercase();
            if let Some((_, other)) = by_email.iter().find(|(e, _)| *e == email) {
                match merged.as_mut() {
                    Some(m) => m.lower(other),
                    None => merged = Some(*other),
                }
            }
        }
        if let Some(c) = merged {
            out.put(alias, c);
        }
    }
    out
}

pub struct Effective {
    pub caps: Caps,
    pub path: Option<String>,
    pub notes: Vec<String>,
    pub unusable: Option<Unusable>,
    pub emails: Emails,
}

/// THE ceiling table: the table in force widened over every alias spelling the
/// store holds; look an alias up in it with `ceiling_of`. With no table, or an
/// unusable one, it is empty.
pub fn effective_caps(store: &Path) -> Effective {
    let ReadCaps {
        caps: table,
        path,
        notes,
        unusable,
    } = read_caps(store);
    if table.is_empty() {
        return Effective {
            caps: table,
            path,
            notes,
            unusable,
            emails: Emails::default(),
        };
    }
    let emails = alias_emails(store);
    let names = known_aliases(store);
    Effective {
        caps: widen(&table, &emails, &names),
        path,
        notes,
        unusable,
        emails,
    }
}

pub struct CapsView {
    pub caps: Caps,
    pub path: Option<String>,
    pub notes: Vec<String>,
    pub unusable: Option<Unusable>,
    /// Accounts at or over a ceiling.
    pub over: Vec<(String, Hit)>,
    /// Accounts NOT over any ceiling that have no current reading of a ceiling
    /// window to be judged by.
    pub unread: Vec<(String, Vec<&'static str>)>,
}

impl CapsView {
    pub fn over_hit(&self, alias: &str) -> Option<&Hit> {
        self.over.iter().find(|(a, _)| a == alias).map(|(_, h)| h)
    }
    pub fn unread_of(&self, alias: &str) -> Option<&Vec<&'static str>> {
        self.unread.iter().find(|(a, _)| a == alias).map(|(_, w)| w)
    }
}

/// Every account at or over its ceiling right now. When `unusable` is set no
/// table applies and `over` and `unread` are empty because nothing was judged,
/// NOT because nothing is capped; every caller must test it before it hands out
/// or ranks an account.
pub fn caps_view(store: &Path, now_ms: f64, max_age_ms: f64, avail: Option<&Avail>) -> CapsView {
    let owned;
    let probes = match avail {
        Some(a) => a,
        None => {
            owned = read_availability(store);
            &owned
        }
    };
    let Effective {
        caps,
        path,
        notes,
        unusable,
        emails,
    } = effective_caps(store);
    let mut over = Vec::new();
    let mut unread = Vec::new();
    if caps.is_empty() {
        return CapsView {
            caps,
            path,
            notes,
            unusable,
            over,
            unread,
        };
    }
    for (alias, ceiling) in &caps.0 {
        let windows = windows_for(alias, probes, &emails, now_ms, max_age_ms);
        if let Some(hit) = over_cap(alias, &windows, &caps, now_ms, max_age_ms) {
            over.push((alias.clone(), hit));
            continue;
        }
        let mut blind: Vec<&'static str> = CAP_WINDOWS
            .into_iter()
            .filter(|n| ceiling.get(n).is_some() && !current(windows.get(n), now_ms, n, max_age_ms))
            .collect();
        blind.sort_unstable();
        if !blind.is_empty() {
            unread.push((alias.clone(), blind));
        }
    }
    CapsView {
        caps,
        path,
        notes,
        unusable,
        over,
        unread,
    }
}

#[derive(Debug)]
pub enum Gate {
    Ok,
    Refuse(Vec<String>),
    Forced(Vec<String>),
}

fn gate_lines(view: &CapsView, key: &str, alias: &str, override_cmd: &str, force: bool) -> Gate {
    if let Some(u) = &view.unusable {
        let reason = format!("the ceiling policy {} {}", u.file, u.why);
        return if force {
            Gate::Forced(vec![format!(
                "override: --force bypassed the ceiling on {alias}; {reason}, so the gate could not check it"
            )])
        } else {
            Gate::Refuse(vec![
                format!("error: refusing '{alias}': {reason}, so no account can be checked against its ceiling"),
                "  Every account is refused until that file is fixed or removed; ASSUME_YES does not lift it.".into(),
                format!("  To go ahead anyway: {override_cmd}"),
            ])
        };
    }
    let hit = view.over_hit(key);
    let blind = view.unread_of(key);
    if hit.is_none() && blind.is_none() {
        return Gate::Ok;
    }
    let path = view.path.clone().unwrap_or_default();
    let overridden = format!("override: --force bypassed the ceiling on {alias}");
    let unknown = blind
        .map(|b| {
            format!(
                "no current reading of its {} window to check it against",
                b.join(" and ")
            )
        })
        .unwrap_or_default();
    if force {
        return Gate::Forced(match hit {
            Some(h) => vec![
                format!(
                    "warning: {alias} is at its ceiling, {} per {path} \u{2014} going ahead because --force was given; cs best passes it over",
                    cap_label(h)
                ),
                overridden,
            ],
            None => vec![format!("{overridden}; {unknown}")],
        });
    }
    let entry = view.caps.get(key).copied().unwrap_or_default();
    let ceilings = if entry.has_broken() {
        "its entry is invalid".to_string()
    } else {
        ["5h", "7d"]
            .iter()
            .filter_map(|n| entry.get(n).map(|v| format!("{n} {v}%")))
            .collect::<Vec<_>>()
            .join(", ")
    };
    Gate::Refuse(match hit {
        Some(h) => vec![
            format!(
                "error: refusing '{alias}': it is at its ceiling, {} per {path}",
                cap_label(h)
            ),
            "  A ceiling is a limit you set on purpose; ASSUME_YES does not lift it.".into(),
            format!("  To go ahead anyway: {override_cmd}"),
        ],
        None => vec![
            format!(
                "error: refusing '{alias}': it has a ceiling ({ceilings}) per {path} and {unknown}"
            ),
            format!(
                "  An unknown reading is treated as over the ceiling. Read it with: sharkswap probe {alias}"
            ),
            format!("  To go ahead anyway: {override_cmd}"),
        ],
    })
}

/// The answer for a launch the user NAMED (`cs <alias>`, `cs pane <alias>`).
/// The table's notes go to stderr. Judged at the real clock and the default
/// probe horizon: `SHARKSWAP_NOW` and `PROBE_MAX_AGE_HOURS` are not read, so
/// no variable in the caller's environment moves it. `quiet` drops the notes
/// (the caller discards stderr). `override_cmd` is the
/// caller's own spelling of the override, for the refusal's last line.
pub fn gate(store: &Path, alias: &str, override_cmd: &str, force: bool, quiet: bool) -> Gate {
    let view = caps_view(
        store,
        crate::util::wall_ms(),
        crate::avail::probe_max_age_ms(""),
        None,
    );
    if !quiet {
        for n in &view.notes {
            eprintln!("caps: note: {n}");
        }
    }
    let key = if view.caps.get(alias).is_some() {
        alias.to_string()
    } else {
        let want = alias.to_lowercase();
        view.caps
            .keys()
            .find(|k| k.to_lowercase() == want)
            .unwrap_or(alias)
            .to_string()
    };
    gate_lines(&view, &key, alias, override_cmd, force)
}

/// For `cs save`: the capped aliases, other than `alias`, that are saved for the
/// login `email`. Saving a capped account's login under a second name would
/// hand out its windows under a name the table does not list. With an UNUSABLE
/// policy every other alias saved for that login counts.
pub fn holders(store: &Path, alias: &str, email: &str) -> Vec<String> {
    let eff = effective_caps(store);
    let unusable = eff.unusable.is_some() || read_caps(store).unusable.is_some();
    let mut out = Vec::new();
    for (other, saved) in alias_emails(store).iter() {
        if other.to_lowercase() != alias.to_lowercase()
            && saved.to_lowercase() == email.to_lowercase()
            && (unusable || ceiling_of(&eff.caps, other).is_some())
        {
            out.push(other.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{H, NOW, TestStore, meta, reading, reading_at};
    use crate::util::wall_ms;
    use serde_json::json;

    const DAY: f64 = 24.0 * H;

    fn five(pct: f64) -> Ceiling {
        Ceiling {
            h5: Some(pct),
            d7: None,
        }
    }

    fn table(entries: &[(&str, Ceiling)]) -> Caps {
        Caps(entries.iter().map(|(k, c)| (k.to_string(), *c)).collect())
    }

    #[test]
    fn read_caps_with_no_file_means_no_ceilings_and_a_usable_policy() {
        let dir = TestStore::new(&[]);
        let c = read_caps(&dir);
        assert!(c.caps.is_empty());
        assert!(c.unusable.is_none());
    }

    #[test]
    fn read_caps_makes_the_whole_policy_unusable_when_the_file_is_not_a_table() {
        let cases = [
            ("", "is not valid JSON"),
            ("{oops", "is not valid JSON"),
            ("{}", "has no \"caps\" object"),
            ("[]", "has no \"caps\" object"),
        ];
        for (body, why) in cases {
            let dir = TestStore::new(&[("account-caps.json", json!(body))]);
            let c = read_caps(&dir);
            assert_eq!(
                c.unusable.as_ref().map(|u| u.why.as_str()),
                Some(why),
                "body {body:?}"
            );
            assert!(c.caps.is_empty());
            assert!(c.notes[0].contains("UNUSABLE"), "body {body:?}");
        }
    }

    #[test]
    fn read_caps_keeps_valid_entries_fails_bad_ones_closed_and_ignores_non_aliases() {
        let dir = TestStore::new(&[(
            "account-caps.json",
            json!({
                "caps": {
                    "Work": { "5h": 80, "7d": 90 },
                    "zero": { "5h": 0 },
                    "over": { "7d": 101 },
                    "window": { "1h": 5 },
                    "empty": {},
                    "text": "50",
                    "../escape": { "5h": 10 },
                    "ok": { "7d": 100 },
                },
            }),
        )]);
        let ReadCaps {
            caps,
            notes,
            unusable,
            ..
        } = read_caps(&dir);
        assert!(unusable.is_none());
        assert_eq!(
            caps.get("work"),
            Some(&Ceiling {
                h5: Some(80.0),
                d7: Some(90.0)
            }),
            "keys are lowercased"
        );
        for bad in ["zero", "over", "window", "empty", "text"] {
            assert_eq!(
                caps.get(bad),
                Some(&Ceiling {
                    h5: Some(BROKEN),
                    d7: Some(BROKEN)
                }),
                "{bad} is capped and always refused"
            );
        }
        assert_eq!(
            caps.get("ok"),
            Some(&Ceiling {
                h5: None,
                d7: Some(100.0)
            })
        );
        assert!(caps.get("../escape").is_none());
        assert!(notes.iter().any(|n| n.contains("not an alias")));
    }

    #[test]
    fn read_caps_merges_entries_that_differ_only_in_case_to_the_lower_ceiling() {
        let dir = TestStore::new(&[(
            "account-caps.json",
            json!({ "caps": { "Work": { "5h": 80, "7d": 95 }, "work": { "5h": 70 } } }),
        )]);
        assert_eq!(
            read_caps(&dir).caps.get("work"),
            Some(&Ceiling {
                h5: Some(70.0),
                d7: Some(95.0)
            })
        );
    }

    #[test]
    fn ceiling_of_is_case_insensitive_and_none_for_an_uncapped_alias() {
        let caps = table(&[("work", five(80.0))]);
        assert_eq!(ceiling_of(&caps, "Work"), Some(five(80.0)));
        assert_eq!(ceiling_of(&caps, "personal"), None);
        assert_eq!(ceiling_of(&Caps::default(), "work"), None);
    }

    fn windows5(pct: f64, reset_ms: Option<f64>, observed_ms: f64) -> Windows {
        Windows(vec![(
            "5h",
            Window {
                used_pct: pct,
                reset_ms,
                observed_ms,
                status: Some("allowed".into()),
            },
        )])
    }

    fn fresh5(pct: f64) -> Windows {
        windows5(pct, Some(NOW + 2.0 * H), NOW - 60_000.0)
    }

    #[test]
    fn over_cap_at_or_over_a_ceiling_is_a_hit_and_under_is_not() {
        let caps = table(&[(
            "work",
            Ceiling {
                h5: Some(80.0),
                d7: Some(90.0),
            },
        )]);
        assert_eq!(
            over_cap("work", &fresh5(85.0), &caps, NOW, DAY),
            Some(Hit {
                window: "5h",
                used_pct: 85.0,
                cap: 80.0,
                reset_ms: Some(NOW + 2.0 * H)
            })
        );
        assert!(
            over_cap("work", &fresh5(80.0), &caps, NOW, DAY).is_some(),
            "exactly at the ceiling counts"
        );
        assert!(over_cap("work", &fresh5(79.0), &caps, NOW, DAY).is_none());
        assert!(
            over_cap("personal", &fresh5(99.0), &caps, NOW, DAY).is_none(),
            "an uncapped alias is never over"
        );
    }

    #[test]
    fn over_cap_rounds_figures_the_way_they_are_shown() {
        let caps = table(&[("work", five(80.0))]);
        assert!(
            over_cap("work", &fresh5(79.6), &caps, NOW, DAY).is_some(),
            "shown as 80%, so it is at an 80% ceiling"
        );
        assert!(over_cap("work", &fresh5(79.4), &caps, NOW, DAY).is_none());
    }

    #[test]
    fn over_cap_says_nothing_for_a_window_that_has_reset_or_is_stamped_in_the_future() {
        let caps = table(&[("work", five(80.0))]);
        assert!(
            over_cap(
                "work",
                &windows5(99.0, Some(NOW - 1.0), NOW - 60_000.0),
                &caps,
                NOW,
                DAY
            )
            .is_none()
        );
        assert!(
            over_cap(
                "work",
                &windows5(99.0, Some(NOW + 2.0 * H), NOW + H),
                &caps,
                NOW,
                DAY
            )
            .is_none()
        );
    }

    #[test]
    fn over_cap_counts_a_reading_with_no_reset_time_for_the_length_of_its_window_within_the_horizon()
     {
        let caps = table(&[("work", five(80.0))]);
        let young = windows5(90.0, None, NOW - 2.0 * H);
        let old = windows5(90.0, None, NOW - 6.0 * H);
        assert!(over_cap("work", &young, &caps, NOW, DAY).is_some());
        assert!(
            over_cap("work", &old, &caps, NOW, DAY).is_none(),
            "5 hours have passed since"
        );
        assert!(
            over_cap("work", &young, &caps, NOW, H).is_none(),
            "the caller's horizon is shorter"
        );
    }

    #[test]
    fn over_cap_names_the_week_when_both_windows_are_over() {
        let caps = table(&[(
            "work",
            Ceiling {
                h5: Some(80.0),
                d7: Some(90.0),
            },
        )]);
        let both = Windows(vec![
            (
                "5h",
                Window {
                    used_pct: 99.0,
                    reset_ms: Some(NOW + H),
                    observed_ms: NOW - 1000.0,
                    status: None,
                },
            ),
            (
                "7d",
                Window {
                    used_pct: 95.0,
                    reset_ms: Some(NOW + 24.0 * H),
                    observed_ms: NOW - 1000.0,
                    status: None,
                },
            ),
        ]);
        assert_eq!(
            over_cap("work", &both, &caps, NOW, DAY).unwrap().window,
            "7d"
        );
    }

    #[test]
    fn over_cap_treats_a_broken_entry_as_over_whatever_the_reading_says() {
        let caps = table(&[(
            "work",
            Ceiling {
                h5: Some(BROKEN),
                d7: Some(BROKEN),
            },
        )]);
        assert!(over_cap("work", &fresh5(0.0), &caps, NOW, DAY).is_some());
        let broken = Hit {
            window: "5h",
            used_pct: 0.0,
            cap: BROKEN,
            reset_ms: None,
        };
        assert!(cap_label(&broken).contains("invalid"));
        let hit = Hit {
            window: "5h",
            used_pct: 96.0,
            cap: 95.0,
            reset_ms: None,
        };
        assert_eq!(cap_label(&hit), "5h 96% (ceiling 95%)");
    }

    #[test]
    fn caps_view_finds_over_unread_and_shared_logins() {
        let dir = TestStore::new(&[
            ("work.json", meta("a@example.com")),
            ("work-copy.json", meta("A@example.com")),
            ("personal.json", meta("b@example.com")),
            ("spare.json", meta("c@example.com")),
            (
                "account-caps.json",
                json!({ "caps": { "work": { "5h": 80 }, "spare": { "7d": 50 } } }),
            ),
            (
                "availability.json",
                json!({ "work": reading(0.9, 0.1), "personal": reading(0.99, 0.99) }),
            ),
        ]);
        let v = caps_view(&dir, NOW, DAY, None);
        assert!(v.unusable.is_none());
        assert_eq!(v.over_hit("work").unwrap().window, "5h");
        assert!(
            v.over_hit("work-copy").is_some(),
            "a second name for a capped login inherits the ceiling and the reading"
        );
        assert!(v.over_hit("personal").is_none(), "no ceiling, never over");
        assert_eq!(
            v.unread_of("spare"),
            Some(&vec!["7d"]),
            "capped and never read"
        );
    }

    #[test]
    fn caps_view_judges_nothing_under_an_unusable_table() {
        let dir = TestStore::new(&[
            ("account-caps.json", json!("{")),
            ("availability.json", json!({ "work": reading(0.99, 0.99) })),
        ]);
        let v = caps_view(&dir, NOW, DAY, None);
        assert!(v.unusable.is_some());
        assert!(v.over.is_empty());
        assert!(v.unread.is_empty());
    }

    #[test]
    fn effective_caps_widens_over_every_alias_saved_for_a_capped_login() {
        let dir = TestStore::new(&[
            ("one.json", meta("x@example.com")),
            ("two.json", meta("X@example.com")),
            ("three.json", meta("y@example.com")),
            (
                "account-caps.json",
                json!({ "caps": { "one": { "5h": 60 } } }),
            ),
        ]);
        let caps = effective_caps(&dir).caps;
        assert_eq!(caps.get("two"), Some(&five(60.0)));
        assert!(caps.get("three").is_none());
    }

    fn text_of(g: &Gate) -> String {
        match g {
            Gate::Ok => String::new(),
            Gate::Refuse(lines) | Gate::Forced(lines) => lines.join("\n"),
        }
    }

    #[test]
    fn gate_is_ok_refuses_or_forces() {
        // The gate judges at the real clock, so the readings are stamped now.
        let now = wall_ms();
        let stamp = |util5: f64| reading_at(util5, 0.1, now - 60_000.0, now);
        let dir = TestStore::new(&[
            ("work.json", meta("a@example.com")),
            ("personal.json", meta("b@example.com")),
            (
                "account-caps.json",
                json!({ "caps": { "work": { "5h": 80 } } }),
            ),
            (
                "availability.json",
                json!({ "work": stamp(0.9), "personal": stamp(0.99) }),
            ),
        ]);
        assert!(matches!(
            gate(&dir, "personal", "cs --force personal", false, true),
            Gate::Ok
        ));
        let refused = gate(&dir, "work", "cs --force work", false, true);
        assert!(matches!(refused, Gate::Refuse(_)), "{refused:?}");
        assert!(text_of(&refused).contains("5h 90% (ceiling 80%)"));
        assert!(text_of(&refused).contains("To go ahead anyway: cs --force work"));
        let forced = gate(&dir, "work", "cs --force work", true, true);
        assert!(matches!(forced, Gate::Forced(_)), "{forced:?}");
        let upper = gate(&dir, "WORK", "cs --force WORK", false, true);
        assert!(
            matches!(upper, Gate::Refuse(_)),
            "the alias is matched without regard to case"
        );
    }

    #[test]
    fn gate_refuses_every_alias_under_an_unusable_table() {
        let dir = TestStore::new(&[("account-caps.json", json!("nope"))]);
        let refused = gate(&dir, "anything", "cs --force anything", false, true);
        assert!(matches!(refused, Gate::Refuse(_)), "{refused:?}");
        assert!(text_of(&refused).contains("not valid JSON"));
        assert!(matches!(
            gate(&dir, "anything", "cs --force anything", true, true),
            Gate::Forced(_)
        ));
    }

    #[test]
    fn gate_with_no_table_at_all_is_ok_for_every_alias() {
        let dir = TestStore::new(&[]);
        assert!(matches!(gate(&dir, "work", "x", false, true), Gate::Ok));
    }

    #[test]
    fn holders_names_the_other_aliases_saved_for_the_same_login() {
        let dir = TestStore::new(&[
            ("work.json", meta("a@example.com")),
            ("old.json", meta("a@example.com")),
            ("personal.json", meta("b@example.com")),
            (
                "account-caps.json",
                json!({ "caps": { "work": { "5h": 80 } } }),
            ),
        ]);
        let mut got = holders(&dir, "new-name", "a@example.com");
        got.sort();
        assert_eq!(got, ["old", "work"]);
        assert!(
            holders(&dir, "new-name", "b@example.com").is_empty(),
            "an uncapped login has no holder"
        );
    }
}
