//! `sharkswap probe`: read each account's usage windows off one tiny request.
//!
//! The Messages API answers every request, a 200 and a 429 alike, with
//! `anthropic-ratelimit-unified-<window>-{status,utilization,reset}` headers for
//! the 5-hour window and the week. So a one-token request is a whole usage
//! reading, for any credential: the one-year token that gets 403 from the usage
//! endpoint gets these headers all the same. A spent account's 429 costs
//! nothing; a 200 costs a few tokens.
//!
//! This only reads credentials. It never refreshes one, never writes one, and
//! never starts `claude`: refreshing a stored login from here would spend its
//! refresh token and leave the snapshot stale.
//!
//! The endpoint is fixed. For tests, `SHARKSWAP_API_BASE` may name a server
//! on this machine (`http://127.0.0.1:PORT`, `http://localhost:PORT` or
//! `http://[::1]:PORT`); any other value is ignored, so an environment variable
//! can never send a token somewhere else.

use crate::store::*;
use crate::util::*;
use serde_json::{Map, Value};
use std::path::Path;
use std::time::Duration;

const DEFAULT_BASE: &str = "https://api.anthropic.com";
const DEFAULT_MODEL: &str = "claude-haiku-4-5-20251001";

/// The API root: the fixed endpoint, or a loopback server named for a test.
pub fn api_base() -> String {
    let raw = env_or_empty("SHARKSWAP_API_BASE");
    if let Some(rest) = raw.strip_prefix("http://") {
        for host in ["127.0.0.1", "localhost", "[::1]"] {
            if let Some(port) = rest.strip_prefix(host).and_then(|r| r.strip_prefix(':'))
                && all_digits(port)
            {
                return raw;
            }
        }
    }
    DEFAULT_BASE.to_string()
}

/// The model the one-token question goes to. The window headers are the
/// account's, not the model's, so the cheapest one will do.
pub fn probe_model() -> String {
    let m = env_or_empty("SHARKSWAP_PROBE_MODEL");
    if m.is_empty() {
        return DEFAULT_MODEL.into();
    }
    if m.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        m
    } else {
        DEFAULT_MODEL.into()
    }
}

/// What a bearer token may contain. Real tokens are base64url with a prefix, so
/// anything else is refused rather than escaped into a header.
pub fn token_chars_ok(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'+' | b'/' | b'=' | b'-')
        })
}

/// The credential a probe may use for an alias.
pub struct Cred {
    pub kind: &'static str,
    pub tok: String,
}

/// Pick the credential a probe may use for `alias`; None when there is none. A
/// usable one-year token wins. Next is whichever stored login has the latest
/// access-token expiry, and only while it has more than a minute left: an
/// expired access token would need a refresh, and a probe that refreshes can
/// spend a snapshot. The live login in ~/.claude counts only when it is this
/// alias's, and only as a file. The macOS Keychain is never read: that
/// prompts, or fails over SSH, and it is the running Claude Code's own
/// credential.
pub fn probe_cred(ctx: &Ctx, alias: &str, cur: &str) -> Option<Cred> {
    if ctx.token_usable(alias) {
        let v = read_last_json(&ctx.token_file(alias));
        let tok = token_text(v.as_ref()).unwrap_or("").to_string();
        if token_chars_ok(&tok) {
            return Some(Cred { kind: "token", tok });
        }
        eprintln!(
            "warning: one-year token for '{alias}' holds characters a header cannot carry; ignoring it"
        );
    }
    let floor = (wall_secs() + 60) as f64 * 1000.0;
    let mut cands: Vec<(&'static str, std::path::PathBuf)> = vec![
        ("snapshot", ctx.snap(alias)),
        ("home", ctx.homes.join(alias).join(".credentials.json")),
    ];
    if !cur.is_empty() && cur == alias {
        cands.push(("active", ctx.creds.clone()));
    }
    let mut best: Option<(f64, Cred)> = None;
    for (kind, f) in cands {
        if !f.is_file() {
            continue;
        }
        let Some((tok, exp)) = file_login(&f) else {
            continue;
        };
        if exp > floor && exp > best.as_ref().map_or(0.0, |b| b.0) {
            best = Some((exp, Cred { kind, tok }));
        }
    }
    best.map(|b| b.1)
}

/// A credentials file's access token (header-safe) and its whole-number expiry.
pub fn file_login(f: &Path) -> Option<(String, f64)> {
    let v = single_json(&read_text(f).ok()?)?;
    let o = v.get("claudeAiOauth")?.as_object()?;
    let tok = o.get("accessToken")?.as_str()?;
    if !token_chars_ok(tok) {
        return None;
    }
    let exp = num(o.get("expiresAt")?)?;
    if !(0.0..=9_007_199_254_740_991.0).contains(&exp) {
        return None;
    }
    Some((tok.to_string(), exp.floor()))
}

// ---- the request ------------------------------------------------------------

/// One reply: the HTTP status (0 for none) and the rate-limit windows it carried.
pub struct Reply {
    pub http: u16,
    /// Window name as sent to `{util, resets, status}`, each field present only
    /// when it validated.
    pub limits: Map<String, Value>,
}

impl Reply {
    pub fn none() -> Reply {
        Reply {
            http: 0,
            limits: Map::new(),
        }
    }
}

fn agent(base: &str, secs: u64) -> ureq::Agent {
    let loopback = base != DEFAULT_BASE;
    let mut cfg = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .timeout_global(Some(Duration::from_secs(secs)))
        .user_agent(concat!("sharkswap/", env!("CARGO_PKG_VERSION")));
    if loopback {
        cfg = cfg.proxy(None);
    }
    cfg.build().into()
}

/// The one-token question, asked with `tok`. A transfer that failed is not a
/// reading: it gives status 0 and no windows.
pub fn ask(tok: &str) -> Reply {
    let base = api_base();
    let body = format!(
        "{{\"model\":\"{}\",\"max_tokens\":1,\"system\":\"You are Claude Code, Anthropic's official CLI for Claude.\",\"messages\":[{{\"role\":\"user\",\"content\":\"hi\"}}]}}",
        probe_model()
    );
    let resp = agent(&base, 30)
        .post(&format!("{base}/v1/messages"))
        .header("Authorization", &format!("Bearer {tok}"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("content-type", "application/json")
        .send(body.as_str());
    let Ok(resp) = resp else { return Reply::none() };
    let code = resp.status().as_u16();
    if !(100..=599).contains(&code) {
        return Reply::none();
    }
    let headers = resp.headers().iter().map(|(k, v)| {
        (
            k.as_str(),
            String::from_utf8_lossy(v.as_bytes()).into_owned(),
        )
    });
    Reply {
        http: code,
        limits: parse_limits(headers),
    }
}

/// The status of the usage endpoint for a token (what `token check` reports),
/// as text: the code, or "000" when nothing came back.
pub fn usage_code(tok: &str, user_agent: &str) -> String {
    let base = api_base();
    let loopback = base != DEFAULT_BASE;
    let mut cfg = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .timeout_global(Some(Duration::from_secs(12)))
        .user_agent(user_agent);
    if loopback {
        cfg = cfg.proxy(None);
    }
    let agent: ureq::Agent = cfg.build().into();
    match agent
        .get(&format!("{base}/api/oauth/usage"))
        .header("Authorization", &format!("Bearer {tok}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .call()
    {
        Ok(r) => format!("{:03}", r.status().as_u16()),
        Err(_) => "000".into(),
    }
}

/// A window is any `<name>` in `anthropic-ratelimit-unified-<name>-{status,utilization,reset}`,
/// kept under the name as sent. Each field is validated on its own and dropped
/// when it fails, so one malformed header cannot take the window's other fields
/// or the verdict with it.
pub fn parse_limits<'a>(headers: impl Iterator<Item = (&'a str, String)>) -> Map<String, Value> {
    const PREFIX: &str = "anthropic-ratelimit-unified-";
    let mut limits: Map<String, Value> = Map::new();
    for (name, value) in headers {
        let name = name.to_ascii_lowercase();
        let Some(rest) = name.strip_prefix(PREFIX) else {
            continue;
        };
        let Some((n, f)) = ["status", "utilization", "reset"].iter().find_map(|f| {
            rest.strip_suffix(f)
                .and_then(|r| r.strip_suffix('-'))
                .map(|n| (n, *f))
        }) else {
            continue;
        };
        if n.is_empty()
            || !n
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            continue;
        }
        let v = value.trim_matches([' ', '\t']).to_ascii_lowercase();
        if v.contains([' ', '\t']) {
            continue;
        }
        let set = |limits: &mut Map<String, Value>, key: &str, val: Value| {
            let w = limits
                .entry(n.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(m) = w {
                m.insert(key.to_string(), val);
            }
        };
        match f {
            "utilization" => {
                let ok = match v.split_once('.') {
                    None => all_digits(&v),
                    Some((a, b)) => all_digits(a) && all_digits(b),
                };
                if ok && let Ok(x) = v.parse::<f64>() {
                    set(&mut limits, "util", json_number(x));
                }
            }
            "reset" => {
                if all_digits(&v)
                    && v.len() <= 11
                    && let Ok(x) = v.parse::<i64>()
                {
                    set(&mut limits, "resets", Value::from(x));
                }
            }
            _ => {
                if !v.is_empty() && v.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
                    set(&mut limits, "status", Value::String(v));
                }
            }
        }
    }
    limits
}

// ---- the verdict -------------------------------------------------------------

pub struct Verdict {
    pub state: &'static str,
    pub reason: &'static str,
    pub resets: Option<f64>,
}

fn rank(name: &str) -> i32 {
    match name {
        "7d" => 3,
        "5h" => 2,
        n if n.contains("opus") || n.contains("sonnet") => 1,
        _ => 0,
    }
}

fn reason_of(name: &str) -> Option<&'static str> {
    match name {
        "7d" => Some("week"),
        "5h" => Some("5h"),
        n if n.contains("opus") || n.contains("sonnet") => Some("model"),
        _ => None,
    }
}

fn win_resets(w: &Value) -> Option<f64> {
    w.get("resets").and_then(Value::as_f64)
}

/// 1. 200: ok.
/// 2. A window reported `rejected`: out, for the rejected window whose reset is
///    LATEST, since that is when the request would go through again. When the
///    resets tie or one is missing the name decides, 7d over 5h over the rest.
///    A name this table does not know is unknown, never guessed into a reason
///    the rotator would act on. `overage` is not a quota window and never takes
///    part. It is still kept in `limits`.
/// 3. Anything else (a 429 with nothing rejected, 401/403, 400, 5xx, no reply)
///    is unknown, and the display cell says which. A failed request says
///    nothing about the account's limits, so it is never recorded as out.
pub fn verdict(r: &Reply) -> Verdict {
    let unknown = Verdict {
        state: "unknown",
        reason: "unknown",
        resets: None,
    };
    if r.http == 200 {
        return Verdict {
            state: "ok",
            reason: "ok",
            resets: None,
        };
    }
    let rej: Vec<(&str, Option<f64>)> = r
        .limits
        .iter()
        .filter(|(k, w)| {
            k.as_str() != "overage" && w.get("status").and_then(Value::as_str) == Some("rejected")
        })
        .map(|(k, w)| (k.as_str(), win_resets(w)))
        .collect();
    if rej.is_empty() {
        return unknown;
    }
    // max_by: the last of equal maxima wins.
    let all_reset = rej.iter().all(|(_, r)| r.is_some());
    let key = |e: &(&str, Option<f64>)| -> (f64, f64) {
        if all_reset {
            (e.1.unwrap_or(-1.0), f64::from(rank(e.0)))
        } else {
            (f64::from(rank(e.0)), e.1.unwrap_or(-1.0))
        }
    };
    let mut w = rej[0];
    for e in &rej[1..] {
        let (a, b) = (key(e), key(&w));
        if a.0 > b.0 || (a.0 == b.0 && a.1 >= b.1) {
            w = *e;
        }
    }
    match reason_of(w.0) {
        None => unknown,
        Some(reason) => Verdict {
            state: "out",
            reason,
            resets: w.1,
        },
    }
}

/// The four display cells of one reply: status, 5h, week and week-reset.
pub fn display(r: &Reply, v: &Verdict, now: i64) -> [String; 4] {
    let cell = if v.state == "ok" {
        "ok".to_string()
    } else {
        match (v.reason, r.http) {
            ("week", _) => "WEEK FULL".into(),
            ("5h", _) => "5H FULL".into(),
            ("model", _) => "MODEL FULL".into(),
            (_, 0) => "no reply".into(),
            (_, c @ (401 | 403)) => format!("AUTH {c}"),
            (_, 429) => "429".into(),
            (_, c) => format!("HTTP {c}"),
        }
    };
    let pct = |name: &str| -> String {
        let Some(w) = r.limits.get(name) else {
            return "-".into();
        };
        let Some(util) = w.get("util").and_then(Value::as_f64) else {
            return "-".into();
        };
        let rejected = w.get("status").and_then(Value::as_str) == Some("rejected");
        format!(
            "{}{}",
            fmt_whole((util * 100.0).round()),
            if rejected { "!" } else { "" }
        )
    };
    let reset = match r.limits.get("7d").and_then(win_resets) {
        None => "-".to_string(),
        Some(at) => {
            let at = at as i64;
            let mut s = local_fmt(at, "%a %H:%M").unwrap_or_default();
            let left = at - now;
            if left > 0 {
                s.push_str(&format!(
                    " in {}d{:02}h",
                    left / 86_400,
                    left % 86_400 / 3600
                ));
            }
            s
        }
    };
    [cell, pct("5h"), pct("7d"), reset]
}

/// A whole-number float as text, without a fraction.
fn fmt_whole(x: f64) -> String {
    if x.is_finite() && x.abs() < 9.0e15 {
        format!("{}", x as i64)
    } else {
        format!("{x}")
    }
}

// ---- one alias ---------------------------------------------------------------

/// One observation for the store.
#[derive(Clone)]
pub struct Obs {
    pub alias: String,
    pub state: &'static str,
    pub reason: &'static str,
    pub resets: Option<f64>,
    pub http: u16,
    pub cred: &'static str,
    pub limits: Map<String, Value>,
}

pub struct Outcome {
    pub obs: Obs,
    pub row: [String; 4],
}

/// Ask for one alias and reduce the reply to an observation and a display row.
pub fn probe_one(alias: &str, cred: &Cred, now: i64) -> Outcome {
    let reply = ask(&cred.tok);
    let v = verdict(&reply);
    let row = display(&reply, &v, now);
    let obs = Obs {
        alias: alias.to_string(),
        state: v.state,
        reason: v.reason,
        resets: v.resets,
        http: reply.http,
        cred: cred.kind,
        limits: reply.limits,
    };
    Outcome { obs, row }
}

// ---- the availability record ---------------------------------------------------

/// Record observations in one read-modify-write under the store lock. Every
/// caller merges here, because a write per observation under parallel probes
/// loses all but the last. Each observation REPLACES the alias's entry, so a
/// probe that learns no reset clears a stale one rather than keeping it alive.
///
/// It fails when nothing was recorded, so `probe` can say so. The current file
/// is read once and only these cases start it afresh: absent, or not a JSON
/// object. A file that exists but cannot be read is left alone.
pub fn avail_record(ctx: &Ctx, obs: &[Obs]) -> Result<(), ()> {
    if obs.is_empty() {
        return Ok(());
    }
    let lock = match IdLock::acquire(ctx) {
        Ok(l) => l,
        Err(_) => {
            eprintln!("sharkswap: availability not recorded");
            return Err(());
        }
    };
    let path = ctx.store.join("availability.json");
    let now = wall_secs();
    let mut cur: Map<String, Value> = Map::new();
    if path.exists() {
        let text = match read_text(&path) {
            Ok(t) => t,
            Err(_) => {
                lock.done();
                eprintln!(
                    "sharkswap: cannot read {}; availability not recorded, file left as it is",
                    path.display()
                );
                return Err(());
            }
        };
        match single_json(trim_nl(&text)) {
            Some(Value::Object(m)) => cur = m,
            _ => {
                let bad = format!("{}.bad-{now}", path.display());
                let nonempty = std::fs::metadata(&path).is_ok_and(|m| m.len() > 0);
                if nonempty && copy_p(&path, Path::new(&bad)).is_ok() {
                    eprintln!(
                        "sharkswap: {} was not one JSON object; kept as {bad}, starting it afresh",
                        path.display()
                    );
                } else {
                    eprintln!(
                        "sharkswap: {} was empty or not a JSON object; starting it afresh",
                        path.display()
                    );
                }
            }
        }
    }
    for o in obs {
        let keep_old = o.state == "unknown"
            && o.http == 0
            && cur.get(&o.alias).is_some_and(|old| {
                old.get("state").and_then(Value::as_str) == Some("out")
                    && old
                        .get("resets")
                        .and_then(Value::as_f64)
                        .is_some_and(|r| r > now as f64)
            });
        if keep_old {
            continue;
        }
        let mut m = Map::new();
        m.insert("state".into(), o.state.into());
        m.insert("reason".into(), o.reason.into());
        m.insert("at".into(), Value::from(now));
        if let Some(r) = o.resets {
            m.insert("resets".into(), json_number(r));
        }
        m.insert("via".into(), "headers".into());
        m.insert("http".into(), Value::from(o.http));
        m.insert("cred".into(), o.cred.into());
        if !o.limits.is_empty() {
            m.insert("limits".into(), Value::Object(o.limits.clone()));
        }
        cur.insert(o.alias.clone(), Value::Object(m));
    }
    let done = !cur.is_empty()
        && write_atomic(&path, format!("{}\n", Value::Object(cur)).as_bytes(), 0o600).is_ok();
    lock.done();
    if done {
        return Ok(());
    }
    eprintln!("sharkswap: could not update {}", path.display());
    Err(())
}

// ---- the command ---------------------------------------------------------------

pub fn cmd_probe(ctx: &Ctx, args: &[String]) -> Result<(), i32> {
    // Readings cannot be recorded while a switch is running or was cut short
    // (see avail_record), so say so before any request is sent.
    crate::switch_intent::ready(&ctx.store)?;
    let mut aliases: Vec<String> = Vec::new();
    let mut from_token = std::collections::BTreeSet::new();
    let named = !args.is_empty();
    if named {
        // A name given twice is probed once.
        for a in args {
            check_alias(a, false)?;
            // Spelled as the store spells it, so a record is never filed under
            // a name the ceiling table would not match.
            alias_exact(a, &[&ctx.store, &ctx.tokens])?;
            if !aliases.contains(a) {
                aliases.push(a.clone());
            }
        }
    } else {
        // Every one-year token, plus every stored login. A login alias with no
        // credential this probe may use gets no row, but it is named once on
        // stderr. A token alias is shown as a row, because a stored token that
        // cannot be used is worth saying.
        for f in glob_json(&ctx.tokens) {
            let a = f[..f.len() - 5].to_string();
            if !alias_ok(&a, false) {
                continue;
            }
            from_token.insert(a.clone());
            aliases.push(a);
        }
        for f in glob_json(&ctx.store) {
            if f.ends_with(".creds.json") || crate::avail::is_own_file(&f) {
                continue;
            }
            if !valid_metadata_file(&ctx.store.join(&f)) {
                continue;
            }
            let a = f[..f.len() - 5].to_string();
            if !alias_ok(&a, false) || from_token.contains(&a) {
                continue;
            }
            aliases.push(a);
        }
        if aliases.is_empty() {
            eprintln!(
                "nothing to probe: no one-year token and no stored login (sharkswap token set <alias>)"
            );
            return Err(1);
        }
        aliases.sort();
    }

    // Credentials are resolved here, one alias at a time, before anything runs:
    // the row order and the "no credential here" rows depend on them. An alias
    // without one is not probed and not recorded, so an earlier good reading of
    // it is not overwritten by what this machine happens not to hold.
    let cur = ctx.current_alias();
    let mut shown: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut jobs: Vec<(String, Cred)> = Vec::new();
    for a in &aliases {
        if let Some(c) = probe_cred(ctx, a, &cur) {
            shown.push(a.clone());
            jobs.push((a.clone(), c));
        } else if named || from_token.contains(a) {
            shown.push(a.clone());
        } else if ctx.snap(a).is_file() || ctx.homes.join(a).join(".credentials.json").is_file() {
            // A login this machine holds but can no longer use. Metadata alone
            // (an account never run here) is the normal case and stays quiet.
            skipped.push(a.clone());
        }
    }

    // One clock for the run, so every row's countdown is measured from the same instant.
    let run_now = wall_secs();
    let outcomes: Vec<Outcome> = std::thread::scope(|s| {
        let handles: Vec<_> = jobs
            .iter()
            .map(|(a, c)| s.spawn(move || probe_one(a, c, run_now)))
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    let mut record_failed = false;
    if !outcomes.is_empty() {
        let mut owned: Vec<Obs> = outcomes.iter().map(|o| o.obs.clone()).collect();
        owned.sort_by(|a, b| a.alias.cmp(&b.alias));
        record_failed = avail_record(ctx, &owned).is_err();
    }

    // Size each column to its own widest cell: a fixed width shifts every later
    // column the moment one cell outgrows it.
    let (mut wa, mut ws, mut w5, mut ww) = (5usize, 6usize, 2usize, 4usize);
    let mut rows: Vec<(String, Option<[String; 4]>)> = Vec::new();
    for a in &shown {
        wa = wa.max(a.chars().count());
        if !jobs.iter().any(|(j, _)| j == a) {
            rows.push((a.clone(), None));
            continue;
        }
        let row = outcomes
            .iter()
            .find(|o| &o.obs.alias == a)
            .map(|o| o.row.clone())
            .unwrap_or_else(|| {
                [
                    "?".to_string(),
                    "?".to_string(),
                    "?".to_string(),
                    "?".to_string(),
                ]
            });
        ws = ws.max(row[0].chars().count());
        w5 = w5.max(row[1].chars().count());
        ww = ww.max(row[2].chars().count());
        rows.push((a.clone(), Some(row)));
    }
    if shown.is_empty() {
        eprintln!("nothing to probe: no alias here has a one-year token or an unexpired login");
        return Err(1);
    }
    println!(
        "{:<wa$}  {:<ws$}  {:<w5$}  {:<ww$}  WEEK-RESET",
        "ALIAS", "STATUS", "5H", "WEEK"
    );
    for (a, row) in &rows {
        match row {
            None => println!("{a:<wa$}  no credential here"),
            Some(r) => println!(
                "{a:<wa$}  {:<ws$}  {:<w5$}  {:<ww$}  {}",
                r[0], r[1], r[2], r[3]
            ),
        }
    }
    if jobs.is_empty() {
        eprintln!(
            "sharkswap probe: no credential on this machine for any of: {}",
            aliases.join(" ")
        );
        return Err(1);
    }
    if !skipped.is_empty() {
        eprintln!(
            "sharkswap probe: not probed, the login stored here has lapsed: {} (sharkswap token set <alias> stores a one-year token)",
            skipped.join(" ")
        );
    }
    if record_failed {
        eprintln!(
            "sharkswap probe: the readings above were NOT recorded; best and the rotator still see the old ones"
        );
        return Err(1);
    }
    Ok(())
}
