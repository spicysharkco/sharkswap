//! What `sharkswap probe` last learned about each account, and the small
//! helpers every reader of the store shares. The record is
//! `$STORE/availability.json`, one entry per alias:
//!
//! ```text
//! { "<alias>": { state: "ok"|"out"|"unknown", reason, at, [resets], via, http, cred, [limits] } }
//! ```
//!
//! * `state`: "ok" means the request was answered. "out" means a usage window
//!   was refused (`reason` says which: "5h", "week" or "model"). "unknown"
//!   means nothing could be concluded (a failed request, a refused credential).
//! * `at`: epoch seconds the reading was taken.
//! * `resets`: epoch seconds the refusal says the refused window reopens.
//! * `limits`: the rate-limit header windows `{ "5h": {util, resets, status}, "7d": {...} }`;
//!   util is a fraction (0..1), resets epoch seconds, status the header word.
//!
//! Nothing here touches the network. The clock is passed in, or is
//! `SHARKSWAP_NOW` (epoch seconds) when that test hook is set, so a view can
//! be pinned in a test; a build without the test hooks never reads it.

use crate::util::{
    all_digits, env_or_empty, is_record, js_number, list_dir, num, read_json, wall_ms,
};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::path::Path;

pub type Avail = Map<String, Value>;

/// Tested as a string, not coerced: a deliberate 0 ("trust nothing I did not
/// just measure") must not turn back into a day.
pub fn probe_max_age_ms(raw: &str) -> f64 {
    let ok = match raw.split_once('.') {
        None => all_digits(raw),
        Some((a, b)) => all_digits(a) && all_digits(b),
    };
    (if ok { js_number(raw) } else { 24.0 }) * 3_600_000.0
}

pub fn max_age_env() -> f64 {
    probe_max_age_ms(&env_or_empty("PROBE_MAX_AGE_HOURS"))
}

/// The whole file, or `{}` when it is missing or is not an object.
pub fn read_availability(store: &Path) -> Avail {
    match read_json(&store.join("availability.json")) {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

/// The instant every judgement here is made at: `raw` (epoch seconds) when it is
/// all digits, the clock otherwise.
pub fn clock_ms_from(raw: &str) -> f64 {
    if all_digits(raw) {
        js_number(raw) * 1000.0
    } else {
        wall_ms()
    }
}

/// The clock, or the `SHARKSWAP_NOW` test hook when it is set.
pub fn clock_ms() -> f64 {
    #[cfg(any(test, feature = "test-hooks"))]
    let raw = crate::store::hook(crate::store::HOOK_NOW).unwrap_or_default();
    #[cfg(not(any(test, feature = "test-hooks")))]
    let raw = String::new();
    clock_ms_from(&raw)
}

/// Files the tool itself writes into the store, which are not account metadata.
pub fn is_own_file(name: &str) -> bool {
    name == "availability.json" || name == "account-caps.json"
}

/// Alias to email, from each account's `<alias>.json` metadata, in file-name order.
#[derive(Default, Clone, Debug)]
pub struct Emails(pub Vec<(String, String)>);

impl Emails {
    pub fn get(&self, alias: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(a, _)| a == alias)
            .map(|(_, e)| e.as_str())
    }
    pub fn iter(&self) -> impl Iterator<Item = &(String, String)> {
        self.0.iter()
    }
}

pub fn alias_emails(store: &Path) -> Emails {
    let mut out = Vec::new();
    for f in list_dir(store) {
        if !f.ends_with(".json") || f.ends_with(".creds.json") || is_own_file(&f) {
            continue;
        }
        let Some(v) = read_json(&store.join(&f)) else {
            continue;
        };
        if let Some(email) = v.get("emailAddress").and_then(Value::as_str)
            && !email.is_empty()
        {
            out.push((f[..f.len() - ".json".len()].to_string(), email.to_string()));
        }
    }
    Emails(out)
}

const YEAR_MS: f64 = 365.0 * 86_400_000.0;

/// Aliases whose one-year token can still authenticate: the file holds a
/// non-empty string token, and the creation time it records, when it records
/// one, is under a year old.
pub fn usable_tokens(store: &Path, now_ms: f64) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for f in list_dir(&store.join("tokens")) {
        if !f.ends_with(".json") {
            continue;
        }
        let Some(tok) = read_json(&store.join("tokens").join(&f)) else {
            continue;
        };
        if tok
            .get("token")
            .and_then(Value::as_str)
            .is_none_or(|t| t.is_empty())
        {
            continue;
        }
        if let Some(created) = tok.get("created").and_then(num)
            && (0.0..1e11).contains(&created)
            && created.floor() * 1000.0 + YEAR_MS <= now_ms
        {
            continue;
        }
        out.insert(f[..f.len() - ".json".len()].to_string());
    }
    out
}

/// Every alias the store holds a trace of: metadata files, one-year token
/// files, saved logins and probe readings, together, sorted.
pub fn known_aliases(store: &Path) -> Vec<String> {
    let mut aliases: BTreeSet<String> = alias_emails(store).0.into_iter().map(|(a, _)| a).collect();
    for f in list_dir(&store.join("tokens")) {
        if let Some(a) = f.strip_suffix(".json") {
            aliases.insert(a.to_string());
        }
    }
    for f in list_dir(store) {
        if let Some(a) = f.strip_suffix(".creds.json") {
            aliases.insert(a.to_string());
        }
    }
    aliases.extend(read_availability(store).keys().cloned());
    aliases.into_iter().collect()
}

/// A window's seconds since the epoch, or None for anything that is not one.
pub fn seconds(v: Option<&Value>) -> Option<f64> {
    v.and_then(num).filter(|n| *n > 0.0)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub used_pct: f64,
    pub reset_ms: Option<f64>,
    pub observed_ms: f64,
    pub status: Option<String>,
}

/// The windows one availability entry measured, "5h" before "7d".
#[derive(Default, Clone, Debug)]
pub struct Windows(pub Vec<(&'static str, Window)>);

impl Windows {
    pub fn get(&self, name: &str) -> Option<&Window> {
        self.0.iter().find(|(n, _)| *n == name).map(|(_, w)| w)
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    fn set(&mut self, name: &'static str, w: Window) {
        match self.0.iter_mut().find(|(n, _)| *n == name) {
            Some(slot) => slot.1 = w,
            None => self.0.push((name, w)),
        }
    }
}

fn status_word(v: Option<&Value>) -> Option<String> {
    let s = v?.as_str()?;
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')).then(|| s.to_string())
}

/// `used_pct` is the header fraction as a percent, capped at 100. A rejected
/// window is 100 whatever its fraction says. A window is dropped when its reset
/// has passed, when the entry is older than `max_age_ms`, or when the entry is
/// stamped ahead of `now_ms`.
pub fn probe_windows(entry: Option<&Value>, now_ms: f64, max_age_ms: f64) -> Windows {
    let mut out = Windows::default();
    let Some(entry) = entry.filter(|e| is_record(e)) else {
        return out;
    };
    let Some(limits) = entry.get("limits").filter(|l| is_record(l)) else {
        return out;
    };
    let Some(at) = seconds(entry.get("at")) else {
        return out;
    };
    let observed_ms = at * 1000.0;
    if observed_ms > now_ms || now_ms - observed_ms >= max_age_ms {
        return out;
    }
    for name in ["5h", "7d"] {
        let Some(l) = limits.get(name).filter(|l| is_record(l)) else {
            continue;
        };
        let status = status_word(l.get("status"));
        let util = l.get("util").and_then(num).filter(|u| *u >= 0.0);
        let rejected = status.as_deref() == Some("rejected");
        if util.is_none() && !rejected {
            continue;
        }
        let reset_ms = seconds(l.get("resets")).map(|s| s * 1000.0);
        if reset_ms.is_some_and(|r| r <= now_ms) {
            continue;
        }
        let used_pct = if rejected {
            100.0
        } else {
            (util.unwrap_or(0.0) * 100.0).min(100.0)
        };
        out.set(
            name,
            Window {
                used_pct,
                reset_ms,
                observed_ms,
                status,
            },
        );
    }
    out
}

/// Two aliases saved for one login draw on one set of windows, so a reading of
/// either describes both. The fresher observation of each window wins, and on a
/// tie the more consumed.
pub fn windows_for(
    alias: &str,
    avail: &Avail,
    emails: &Emails,
    now_ms: f64,
    max_age_ms: f64,
) -> Windows {
    let email = emails.get(alias).map(str::to_lowercase);
    let mut names = vec![alias.to_string()];
    if let Some(email) = &email {
        for (other, e) in emails.iter() {
            if other != alias && e.to_lowercase() == *email {
                names.push(other.clone());
            }
        }
    }
    let mut out = Windows::default();
    for name in names {
        for (win, w) in probe_windows(avail.get(&name), now_ms, max_age_ms).0 {
            let better = match out.get(win) {
                None => true,
                Some(h) => {
                    w.observed_ms > h.observed_ms
                        || (w.observed_ms == h.observed_ms && w.used_pct > h.used_pct)
                }
            };
            if better {
                out.set(win, w);
            }
        }
    }
    out
}

/// How far one entry can be believed at `now_ms`: its age in ms, or None when
/// it has no usable stamp. A stamp ahead of the clock is 0, not a negative age.
pub fn age_ms(entry: Option<&Value>, now_ms: f64) -> Option<f64> {
    seconds(entry.and_then(|e| e.get("at"))).map(|at| (now_ms - at * 1000.0).max(0.0))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    None,
    Ok,
    Out,
}

/// Whether an entry still says its account is refused: state "out" and the
/// reset it named, when it named one, is ahead.
pub fn standing(entry: Option<&Value>, now_ms: f64, max_age_ms: f64) -> Standing {
    let Some(entry) = entry.filter(|e| is_record(e)) else {
        return Standing::None;
    };
    match age_ms(Some(entry), now_ms) {
        Some(age) if age < max_age_ms => {}
        _ => return Standing::None,
    }
    let state = entry.get("state").and_then(Value::as_str);
    if state == Some("out") {
        return match seconds(entry.get("resets")) {
            Some(r) if r * 1000.0 <= now_ms => Standing::Ok,
            _ => Standing::Out,
        };
    }
    if state == Some("ok") {
        Standing::Ok
    } else {
        Standing::None
    }
}

/// Age of a reading, coarse and clamped: "3h", or "2d" past 48 hours.
pub fn fmt_age(ms: Option<f64>) -> String {
    let Some(ms) = ms else { return String::new() };
    let h = ms.max(0.0) / 3_600_000.0;
    if h < 48.0 {
        format!("{}h", crate::util::js_round(h))
    } else {
        format!("{}d", crate::util::js_round(h / 24.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{H, NOW, TestStore, avail, meta, reading, reading_at, sec};
    use serde_json::json;

    const DAY: f64 = 24.0 * H;

    #[test]
    fn probe_max_age_is_a_day_by_default_a_plain_number_of_hours_when_given_and_zero_is_honoured() {
        assert_eq!(probe_max_age_ms(""), 24.0 * H);
        assert_eq!(probe_max_age_ms("6"), 6.0 * H);
        assert_eq!(probe_max_age_ms("1.5"), 1.5 * H);
        assert_eq!(probe_max_age_ms("0"), 0.0);
        assert_eq!(probe_max_age_ms("soon"), 24.0 * H);
        assert_eq!(probe_max_age_ms("-3"), 24.0 * H);
    }

    #[test]
    fn clock_is_whole_epoch_seconds_and_anything_else_is_the_real_clock() {
        assert_eq!(clock_ms_from("1800000000"), NOW);
        assert!((clock_ms_from("") - wall_ms()).abs() < 5000.0);
        assert!((clock_ms_from("yesterday") - wall_ms()).abs() < 5000.0);
    }

    #[test]
    fn is_record_means_objects_only() {
        assert!(is_record(&json!({})));
        assert!(!is_record(&json!([])));
        assert!(!is_record(&Value::Null));
        assert!(!is_record(&json!("x")));
    }

    #[test]
    fn probe_windows_turns_fractions_into_percents_capped_at_100() {
        let w = probe_windows(Some(&reading(0.4, 1.01)), NOW, DAY);
        assert_eq!(w.get("5h").unwrap().used_pct, 40.0);
        assert_eq!(w.get("7d").unwrap().used_pct, 100.0);
        assert_eq!(w.get("5h").unwrap().reset_ms, Some(NOW + 2.0 * H));
    }

    #[test]
    fn probe_windows_counts_a_rejected_window_as_spent_whatever_its_fraction_says() {
        let mut e = reading(0.2, 0.3);
        e["limits"]["5h"]["status"] = json!("rejected");
        assert_eq!(
            probe_windows(Some(&e), NOW, DAY)
                .get("5h")
                .unwrap()
                .used_pct,
            100.0
        );
    }

    #[test]
    fn probe_windows_gives_nothing_for_a_stale_future_stamped_or_malformed_entry() {
        let old = reading_at(0.4, 0.4, NOW - 25.0 * H, NOW);
        assert!(
            probe_windows(Some(&old), NOW, DAY).is_empty(),
            "older than the horizon"
        );
        let ahead = reading_at(0.4, 0.4, NOW + 5.0 * 60_000.0, NOW);
        assert!(
            probe_windows(Some(&ahead), NOW, DAY).is_empty(),
            "stamped ahead of the clock"
        );
        assert!(probe_windows(None, NOW, DAY).is_empty());
        assert!(
            probe_windows(Some(&json!({ "at": sec(NOW), "limits": "x" })), NOW, DAY).is_empty()
        );
        assert!(
            probe_windows(
                Some(&json!({ "limits": { "5h": { "util": 0.1 } } })),
                NOW,
                DAY
            )
            .is_empty(),
            "no stamp"
        );
    }

    #[test]
    fn probe_windows_drops_a_window_past_its_reset() {
        let mut e = reading(0.9, 0.9);
        e["limits"]["5h"]["resets"] = json!(sec(NOW - 1000.0));
        let w = probe_windows(Some(&e), NOW, DAY);
        assert!(w.get("5h").is_none());
        assert!(w.get("7d").is_some());
    }

    #[test]
    fn windows_for_shares_windows_between_aliases_of_one_login_and_the_fresher_reading_wins() {
        let emails = Emails(vec![
            ("work".into(), "A@Example.com".into()),
            ("work2".into(), "a@example.com".into()),
            ("other".into(), "b@example.com".into()),
        ]);
        let readings = avail(json!({
            "work": reading_at(0.2, 0.2, NOW - 3.0 * H, NOW),
            "work2": reading_at(0.7, 0.3, NOW - 60_000.0, NOW),
            "other": reading(0.9, 0.9),
        }));
        let w = windows_for("work", &readings, &emails, NOW, DAY);
        assert_eq!(
            w.get("5h").unwrap().used_pct,
            70.0,
            "work2's newer reading describes work too"
        );
        assert_eq!(
            windows_for("other", &readings, &emails, NOW, DAY)
                .get("5h")
                .unwrap()
                .used_pct,
            90.0
        );
    }

    #[test]
    fn standing_covers_ok_out_with_its_reset_and_an_old_entry() {
        let max = DAY;
        assert_eq!(
            standing(
                Some(&json!({ "state": "ok", "at": sec(NOW - H) })),
                NOW,
                max
            ),
            Standing::Ok
        );
        let out = json!({ "state": "out", "at": sec(NOW - H), "resets": sec(NOW + H) });
        assert_eq!(standing(Some(&out), NOW, max), Standing::Out);
        let reset = json!({ "state": "out", "at": sec(NOW - H), "resets": sec(NOW - 60_000.0) });
        assert_eq!(
            standing(Some(&reset), NOW, max),
            Standing::Ok,
            "its reset has passed"
        );
        let no_reset = json!({ "state": "out", "at": sec(NOW - H) });
        assert_eq!(
            standing(Some(&no_reset), NOW, max),
            Standing::Out,
            "no reset: holds for the horizon"
        );
        let old = json!({ "state": "out", "at": sec(NOW - 30.0 * H) });
        assert_eq!(standing(Some(&old), NOW, max), Standing::None);
        assert_eq!(
            standing(
                Some(&json!({ "state": "unknown", "at": sec(NOW) })),
                NOW,
                max
            ),
            Standing::None
        );
        assert_eq!(standing(None, NOW, max), Standing::None);
    }

    #[test]
    fn age_and_fmt_age() {
        assert_eq!(age_ms(Some(&json!({ "at": sec(NOW - H) })), NOW), Some(H));
        assert_eq!(
            age_ms(Some(&json!({ "at": sec(NOW + H) })), NOW),
            Some(0.0),
            "a stamp ahead of the clock is age 0"
        );
        assert_eq!(age_ms(Some(&json!({})), NOW), None);
        assert_eq!(fmt_age(Some(3.0 * H)), "3h");
        assert_eq!(fmt_age(Some(72.0 * H)), "3d");
        assert_eq!(fmt_age(None), "");
    }

    #[test]
    fn usable_tokens_need_a_token_string_and_under_a_year_old_when_they_say_when() {
        let day = 86_400_000.0;
        let dir = TestStore::new(&[
            (
                "tokens/fresh.json",
                json!({ "token": "t", "created": sec(NOW - 10.0 * day) }),
            ),
            (
                "tokens/old.json",
                json!({ "token": "t", "created": sec(NOW - 400.0 * day) }),
            ),
            (
                "tokens/empty.json",
                json!({ "token": "", "created": sec(NOW) }),
            ),
            ("tokens/undated.json", json!({ "token": "t" })),
            ("tokens/broken.json", json!("{not json")),
        ]);
        let got: Vec<String> = usable_tokens(&dir, NOW).into_iter().collect();
        assert_eq!(got, ["fresh", "undated"]);
    }

    #[test]
    fn known_aliases_pools_metadata_tokens_snapshots_and_readings_and_skips_the_tools_own_files() {
        let dir = TestStore::new(&[
            ("alpha.json", meta("a@example.com")),
            ("bravo.creds.json", json!({})),
            ("tokens/charlie.json", json!({ "token": "t" })),
            ("availability.json", json!({ "delta": reading(0.1, 0.1) })),
            ("account-caps.json", json!({ "caps": {} })),
        ]);
        assert_eq!(known_aliases(&dir), ["alpha", "bravo", "charlie", "delta"]);
    }
}
