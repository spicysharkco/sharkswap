//! The commands that read or move the live login: status, list, save, remove,
//! switch, pane, watch, token, best, bar, overview, next.

use crate::avail::is_own_file;
use crate::caps::{self, Gate};
use crate::probe;
use crate::store::*;
use crate::switch_intent;
use crate::util::*;
use serde_json::{Map, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

type Res = Result<(), i32>;

// ---- shared helpers -----------------------------------------------------------

/// `--force` may come anywhere; the first other word is the alias.
fn alias_force(args: &[String]) -> (String, bool) {
    let (mut alias, mut force) = (String::new(), false);
    for a in args {
        if a == "--force" {
            force = true;
        } else if alias.is_empty() {
            alias = a.clone();
        }
    }
    (alias, force)
}

/// Account aliases in the store: snapshot files that are neither a saved login
/// nor one of the tool's own files.
fn account_aliases(ctx: &Ctx) -> Vec<String> {
    glob_json(&ctx.store)
        .into_iter()
        .filter(|f| !f.ends_with(".creds.json") && !is_own_file(f))
        .map(|f| f[..f.len() - ".json".len()].to_string())
        .collect()
}

/// The ceiling gate for a launch the user named. Refusal is exit 75, with the
/// lines on stderr; `quiet` is for callers that discard stderr.
fn shared_gate(ctx: &Ctx, alias: &str, override_cmd: &str, force: bool, quiet: bool) -> Res {
    match caps::gate(&ctx.store, alias, override_cmd, force, quiet) {
        Gate::Ok => Ok(()),
        Gate::Forced(lines) => {
            if !quiet {
                for l in lines {
                    eprintln!("{l}");
                }
            }
            Ok(())
        }
        Gate::Refuse(lines) => {
            if !quiet {
                for l in lines {
                    eprintln!("{l}");
                }
            }
            Err(75)
        }
    }
}

/// `jq -r '.emailAddress'` for a metadata value.
fn email_of(v: Option<&Value>) -> String {
    jq_text(v.and_then(|v| v.get("emailAddress")))
}

/// Pretty JSON the way `jq .` prints it, newline included.
fn pretty(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).unwrap_or_default();
    s.push('\n');
    s
}

/// `jq '{claudeAiOauth}'` over a credentials value.
fn oauth_only(v: &Value) -> Value {
    let mut m = Map::new();
    m.insert(
        "claudeAiOauth".into(),
        v.get("claudeAiOauth").cloned().unwrap_or(Value::Null),
    );
    Value::Object(m)
}

/// Say where each copy of a previous file is kept, so that nobody has to guess
/// at a name that has a random tail.
fn name_kept_copies<'a>(copies: impl IntoIterator<Item = &'a PathBuf>) {
    for k in copies {
        eprintln!(
            "error: a copy of the previous file is kept at {}",
            k.display()
        );
    }
}

/// `jq --argjson b BLOCK '.oauthAccount = $b'` over a file's text. None when
/// jq would fail (not JSON, not an object) or print nothing at all.
fn with_account(text: &str, block: &Value) -> Option<String> {
    let mut out = String::new();
    for v in serde_json::Deserializer::from_str(text).into_iter::<Value>() {
        let mut v = v.ok()?;
        match &mut v {
            Value::Object(o) => {
                o.insert("oauthAccount".into(), block.clone());
            }
            Value::Null => {
                let mut m = Map::new();
                m.insert("oauthAccount".into(), block.clone());
                v = Value::Object(m);
            }
            _ => return None,
        }
        out.push_str(&pretty(&v));
    }
    if out.is_empty() { None } else { Some(out) }
}

fn is_tty(fd: i32) -> bool {
    // SAFETY: isatty only inspects a descriptor number.
    unsafe { libc::isatty(fd) == 1 }
}

fn hms(secs: i64) -> String {
    local_fmt(secs, "%H:%M:%S").unwrap_or_default()
}

fn locale_utf8() -> bool {
    for k in ["LC_ALL", "LC_CTYPE", "LANG"] {
        let v = env_or_empty(k);
        if !v.is_empty() {
            let l = v.to_ascii_lowercase();
            return l.contains("utf-8") || l.contains("utf8");
        }
    }
    false
}

/// Bash's `printf %q`: a word a shell reads back as the same bytes.
pub fn bash_quote(b: &[u8]) -> String {
    if b.is_empty() {
        return "''".into();
    }
    let utf8 = locale_utf8() && std::str::from_utf8(b).is_ok();
    let ansi = b
        .iter()
        .any(|&c| c < 0x20 || c == 0x7f || (c >= 0x80 && !utf8));
    let mut out = String::new();
    if ansi {
        let mut raw: Vec<u8> = Vec::new();
        for &c in b {
            match c {
                7 => raw.extend_from_slice(b"\\a"),
                8 => raw.extend_from_slice(b"\\b"),
                0x1b => raw.extend_from_slice(b"\\E"),
                12 => raw.extend_from_slice(b"\\f"),
                10 => raw.extend_from_slice(b"\\n"),
                13 => raw.extend_from_slice(b"\\r"),
                9 => raw.extend_from_slice(b"\\t"),
                11 => raw.extend_from_slice(b"\\v"),
                b'\\' => raw.extend_from_slice(b"\\\\"),
                b'\'' => raw.extend_from_slice(b"\\'"),
                c if c < 0x20 || c == 0x7f || (c >= 0x80 && !utf8) => {
                    raw.extend_from_slice(format!("\\{c:03o}").as_bytes())
                }
                c => raw.push(c),
            }
        }
        out.push_str("$'");
        out.push_str(&String::from_utf8_lossy(&raw));
        out.push('\'');
        return out;
    }
    let s = String::from_utf8_lossy(b);
    for (i, ch) in s.chars().enumerate() {
        let special = matches!(
            ch,
            ' ' | '!'
                | '"'
                | '$'
                | '&'
                | '\''
                | '('
                | ')'
                | '*'
                | ','
                | ';'
                | '<'
                | '>'
                | '?'
                | '['
                | '\\'
                | ']'
                | '^'
                | '`'
                | '{'
                | '|'
                | '}'
        ) || (i == 0 && (ch == '~' || ch == '#'));
        if special {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// printf `%-Ns`: pads by bytes, as bash does.
fn pad_r(s: &str, width: usize) -> String {
    let mut o = s.to_string();
    for _ in s.len()..width {
        o.push(' ');
    }
    o
}

// ---- status ---------------------------------------------------------------------

pub fn cmd_status(ctx: &Ctx) -> Res {
    // An unfinished switch leaves the live identity uncertain: say nothing about it.
    switch_intent::ready(&ctx.store)?;
    let email = ctx.current_email();
    let alias = ctx.current_alias();
    println!(
        "Active email : {}",
        if email.is_empty() { "(none)" } else { &email }
    );
    println!(
        "Active alias : {}",
        if alias.is_empty() {
            "(unregistered)"
        } else {
            &alias
        }
    );
    if !email.is_empty() && alias.is_empty() {
        println!(
            "               \u{21b3} unregistered: the watcher cannot snapshot this account's token."
        );
        println!("                 Run: sharkswap save <alias>");
    }
    let live = ctx.creds_read();
    if !live.is_empty() {
        let v = last_json(&live);
        println!("Token expiry : {}", token_expiry_of(v.as_ref()));
        println!("Login expiry : {}", login_expiry_of(v.as_ref()));
        let sub = match &v {
            Some(Value::Object(o)) => match o.get("claudeAiOauth") {
                None | Some(Value::Null) => Some(Value::Null),
                Some(Value::Object(i)) => {
                    Some(i.get("subscriptionType").cloned().unwrap_or(Value::Null))
                }
                Some(_) => None,
            },
            Some(Value::Null) => Some(Value::Null),
            _ => None,
        };
        let shown = match sub {
            None => String::new(),
            Some(Value::Null) | Some(Value::Bool(false)) => "?".into(),
            Some(Value::String(s)) => s,
            Some(other) => serde_json::to_string_pretty(&other).unwrap_or_default(),
        };
        println!("Subscription : {shown}");
    } else {
        let more = if ctx.darwin {
            ", or Keychain locked"
        } else {
            ""
        };
        println!("Token        : (none \u{2014} not logged in{more})");
    }
    println!("Store        : {}", ctx.store.display());

    let quick = env_or_empty("QUICK")
        .trim()
        .parse::<i64>()
        .is_ok_and(|n| n != 0);
    if quick || alias.is_empty() {
        return Ok(());
    }
    println!();
    let _ = probe::cmd_probe(ctx, std::slice::from_ref(&alias));
    Ok(())
}

// ---- list -----------------------------------------------------------------------

pub fn cmd_list(ctx: &Ctx) -> Res {
    let aliases = account_aliases(ctx);
    if aliases.is_empty() {
        println!("No accounts saved. Use: sharkswap save <alias>");
        return Ok(());
    }
    let active = ctx.current_alias();
    // Last-known usage per account; nothing is asked of the network.
    let used = if is_file(&ctx.store.join("availability.json")) {
        crate::views::list_cells(&ctx.store)
    } else {
        Vec::new()
    };
    println!(
        "Accounts (\u{2713} = token captured, \u{2717} = saved login expired, T = one-year token, USED = 5h/week as of the last 'sharkswap probe', LOGIN = login left):"
    );
    for a in &aliases {
        let meta = ctx.meta(a);
        let email = match read_last_json(&meta) {
            Some(Value::Object(o)) => match o.get("emailAddress") {
                None | Some(Value::Null) | Some(Value::Bool(false)) => "?".to_string(),
                v => jq_text(v),
            },
            _ => "?".to_string(),
        };
        let (mut tok, mut login) = (" ", String::new());
        let snap = ctx.snap(a);
        if is_file(&snap) {
            tok = "\u{2713}";
            login = ctx.login_left_short(&snap);
            if login == "EXPIRED" {
                tok = "\u{2717}";
            }
        }
        let tf = ctx.token_file(a);
        if is_file(&tf) {
            tok = "T";
            login = format!("year {}", ctx.token_left_short(&tf));
        }
        let mark = if *a == active {
            "  \u{2190} active"
        } else {
            ""
        };
        let cell = used
            .iter()
            .find(|(u, _)| u == a)
            .map(|(_, c)| c.as_str())
            .unwrap_or("");
        let line = format!(
            "  {tok} {} {} {} {}{mark}",
            pad_r(a, 12),
            pad_r(&email, 30),
            pad_r(cell, 9),
            pad_r(&login, 8)
        );
        println!(
            "{}",
            line.trim_end_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c'])
        );
    }
    Ok(())
}

// ---- save and remove --------------------------------------------------------------

pub fn cmd_save(ctx: &Ctx, args: &[String]) -> Res {
    let (alias, force) = alias_force(args);
    if alias.is_empty() {
        eprintln!("sharkswap: alias: alias required");
        return Err(1);
    }
    check_alias(&alias, false)?;
    alias_exact(&alias, &[&ctx.store, &ctx.tokens])?;
    let lock = IdLock::acquire(ctx)?;
    // A login already saved under a capped alias is not saved again under another name.
    // The live login is read under the lock: a running switch hides it until it ends.
    // A refusal below drops the lock.
    let live_email = ctx.current_email();
    let holder = if live_email.is_empty() {
        None
    } else {
        caps::holders(&ctx.store, &alias, &live_email)
            .into_iter()
            .next()
    };
    if let Some(h) = holder {
        if force {
            eprintln!(
                "override: --force saved '{alias}' although this login is already saved as '{h}', which has a ceiling"
            );
        } else {
            eprintln!(
                "error: refusing to save '{alias}': this login is already saved as '{h}', which has a ceiling"
            );
            eprintln!(
                "  A second name for it would hand out the same windows under a name the table does not list."
            );
            eprintln!("  To save it anyway: sharkswap save --force {alias}");
            return Err(75);
        }
    }
    let meta_dst = ctx.meta(&alias);
    let creds_dst = ctx.snap(&alias);
    let block = read_last_json(&ctx.files[0])
        .and_then(|v| match v {
            Value::Object(o) => o.get("oauthAccount").cloned(),
            _ => None,
        })
        .filter(|b| !matches!(b, Value::Null | Value::Bool(false)));
    let Some(block) = block.filter(valid_metadata) else {
        eprintln!(
            "error: missing or malformed oauthAccount in {}",
            ctx.files[0].display()
        );
        return Err(1);
    };
    let live = ctx.creds_read();
    let live_v = if live.is_empty() {
        None
    } else {
        last_json(&live)
    };
    let Some(live_v) = live_v.filter(valid_oauth) else {
        eprintln!("error: no valid live OAuth token; account snapshot was not changed");
        return Err(1);
    };
    let prefix = |kind: &str| ctx.store.join(format!(".{alias}.{kind}."));
    let Ok(meta_bak) = mktemp(prefix("meta-bak")) else {
        return Err(1);
    };
    let Ok(creds_bak) = mktemp(prefix("creds-bak")) else {
        rm_f(&meta_bak);
        return Err(1);
    };
    let clean = || {
        rm_f(&meta_bak);
        rm_f(&creds_bak);
    };
    let (meta_text, creds_text) = (pretty(&block), pretty(&oauth_only(&live_v)));
    let (mut meta_had, mut creds_had) = (false, false);
    if meta_dst.exists() {
        meta_had = true;
        if copy_p(&meta_dst, &meta_bak).is_err() {
            clean();
            eprintln!("error: could not back up account metadata");
            return Err(1);
        }
    }
    if creds_dst.exists() {
        creds_had = true;
        if copy_p(&creds_dst, &creds_bak).is_err() {
            clean();
            eprintln!("error: could not back up account credentials");
            return Err(1);
        }
    }
    if write_atomic(&meta_dst, meta_text.as_bytes(), 0o600).is_err()
        || write_atomic(&creds_dst, creds_text.as_bytes(), 0o600).is_err()
    {
        restore_path(&meta_dst, &meta_bak, meta_had);
        restore_path(&creds_dst, &creds_bak, creds_had);
        clean();
        eprintln!("error: account snapshot commit failed; restored prior state where possible");
        return Err(1);
    }
    rm_f(&meta_bak);
    rm_f(&creds_bak);
    let email = email_of(read_last_json(&meta_dst).as_ref());
    lock.done();
    println!(
        "saved metadata  {alias} \u{2192} {} ({email})",
        meta_dst.display()
    );
    println!(
        "saved token     {alias} \u{2192} {} (expiry {})",
        creds_dst.display(),
        ctx.token_expiry(&creds_dst)
    );
    Ok(())
}

pub fn cmd_remove(ctx: &Ctx, args: &[String]) -> Res {
    let alias = args.first().cloned().unwrap_or_default();
    if alias.is_empty() {
        eprintln!("sharkswap: 1: alias required");
        return Err(1);
    }
    check_alias(&alias, true)?;
    alias_exact(&alias, &[&ctx.store, &ctx.tokens])?;
    let home = ctx.homes.join(&alias);
    let home_there = std::fs::symlink_metadata(&home).is_ok();
    if !ctx.meta(&alias).exists()
        && !ctx.snap(&alias).exists()
        && !ctx.token_file(&alias).exists()
        && !home.is_dir()
    {
        eprintln!("error: unknown account '{alias}'");
        return Err(1);
    }
    let lock = IdLock::acquire(ctx)?;
    rm_f(&ctx.meta(&alias));
    rm_f(&ctx.snap(&alias));
    rm_f(&ctx.token_file(&alias));
    if home_there {
        if home.is_dir() && !home.is_symlink() {
            let _ = std::fs::remove_dir_all(&home);
        } else {
            let _ = std::fs::remove_file(&home);
        }
    }
    lock.done();
    println!("removed {alias}");
    Ok(())
}

// ---- switch ---------------------------------------------------------------------

/// Write `prior` to a new 0600 file in the store, for a Keychain put-back that
/// failed. None when even that cannot be written.
fn save_prior_login(ctx: &Ctx, prior: &str) -> Option<PathBuf> {
    let path = mktemp(ctx.store.join("keychain-prior-login.")).ok()?;
    if write_atomic(&path, format!("{prior}\n").as_bytes(), 0o600).is_err() {
        rm_f(&path);
        return None;
    }
    Some(path)
}

/// Undo the creation of a credentials file that did not exist before the
/// switch. `rm_f` ignores errors, so a file still there afterwards is named: it
/// holds the new login, and the run must not report the old state as restored.
fn remove_new_creds(creds: &Path, not_restored: &mut Vec<String>) {
    rm_f(creds);
    if std::fs::symlink_metadata(creds).is_ok() {
        not_restored.push(creds.display().to_string());
    }
}

fn apply_block(ctx: &Ctx, alias: &str) -> Res {
    let src = ctx.meta(alias);
    let lock = IdLock::acquire(ctx)?;
    // `rotate` names the login its decision was made from. It judged that before
    // probing and before this process took the lock, so it is checked again here,
    // before anything is written. The lock excludes only other sharkswap switches,
    // not `claude /login` or a token refresh: this narrows the window, it does not
    // close it. Unset for every other caller.
    let expect = env_or_empty(EXPECT_FROM);
    if !expect.is_empty() {
        let now = ctx.current_email();
        if now != expect {
            eprintln!(
                "error: the active login is '{}', not '{}'; nothing was changed (set by {EXPECT_FROM})",
                ctx.login_name(&now),
                ctx.login_name(&expect)
            );
            return Err(EXIT_MOVED);
        }
    }
    let tok_block = ctx.token_oauth_block(alias);
    if !valid_metadata_file(&src) || (tok_block.is_none() && !valid_oauth_file(&ctx.snap(alias))) {
        eprintln!("error: account '{alias}' has malformed metadata or credentials");
        return Err(1);
    }
    if !is_file(&ctx.files[0]) {
        eprintln!("error: primary live configuration is missing; refusing a token-only switch");
        return Err(1);
    }
    let Some(block) = read_last_json(&src) else {
        eprintln!(
            "error: cannot read account metadata {}: unreadable or no longer valid JSON",
            src.display()
        );
        return Err(1);
    };
    let oauth = match tok_block {
        Some(b) => b,
        None => {
            match read_last_json(&ctx.snap(alias)).and_then(|v| v.get("claudeAiOauth").cloned()) {
                Some(o) => o,
                None => {
                    eprintln!(
                        "error: cannot find the claudeAiOauth block in {}",
                        ctx.snap(alias).display()
                    );
                    return Err(1);
                }
            }
        }
    };
    // The new text of each live configuration file, checked now and written
    // only after the credentials are, so a bad file stops the switch before
    // anything changes.
    let mut staged: Vec<String> = Vec::new();
    let mut backups: Vec<PathBuf> = Vec::new();
    let mut destinations: Vec<PathBuf> = Vec::new();
    let cleanup = |backups: &[PathBuf]| {
        for p in backups {
            rm_f(p);
        }
    };
    for file in &ctx.files {
        if !is_file(file) {
            continue;
        }
        let f = file.display();
        let Ok(backup) = mktemp(suffixed(file, ".backup.")) else {
            cleanup(&backups);
            eprintln!("error: could not back up live configuration {f}");
            return Err(1);
        };
        let out = match read_text(file) {
            Err(e) => Err(os_msg(&e)),
            Ok(t) => with_account(&t, &block).ok_or_else(|| "not a JSON object".to_string()),
        }
        .and_then(|out| match copy_p(file, &backup) {
            Ok(()) => Ok(out),
            Err(e) => Err(format!("cannot copy it: {}", os_msg(&e))),
        });
        let out = match out {
            Ok(out) => out,
            Err(why) => {
                rm_f(&backup);
                cleanup(&backups);
                eprintln!("error: could not prevalidate live configuration {f}: {why}");
                return Err(1);
            }
        };
        staged.push(out);
        backups.push(backup);
        destinations.push(file.clone());
    }
    let mut keychain = false;
    let mut prior_creds = String::new();
    let mut creds_backup: Option<PathBuf> = None;
    let mut creds_had = false;
    let live: String;
    if ctx.darwin && !ctx.creds.is_file() {
        keychain = true;
        match keychain_prior().filter(|p| last_json(p).is_some_and(|v| v.is_object())) {
            Some(p) => {
                prior_creds = p.clone();
                live = p;
            }
            None => {
                cleanup(&backups);
                eprintln!(
                    "error: cannot read prior Keychain credentials; refusing a switch that cannot be rolled back"
                );
                return Err(1);
            }
        }
    } else {
        live = ctx.creds_read();
        if !live.is_empty() && !last_json(&live).is_some_and(|v| v.is_object()) {
            cleanup(&backups);
            eprintln!("error: live credentials are malformed; refusing to replace them");
            return Err(1);
        }
        let cr = ctx.creds.display();
        let cb = match mktemp(suffixed(&ctx.creds, ".switch-backup.")) {
            Ok(cb) => cb,
            Err(e) => {
                cleanup(&backups);
                eprintln!(
                    "error: could not back up live credentials {cr}: cannot make a backup file: {}",
                    os_msg(&e)
                );
                return Err(1);
            }
        };
        if ctx.creds.exists() {
            creds_had = true;
            if let Err(e) = copy_p(&ctx.creds, &cb) {
                rm_f(&cb);
                cleanup(&backups);
                eprintln!(
                    "error: could not back up live credentials {cr}: cannot copy it: {}",
                    os_msg(&e)
                );
                return Err(1);
            }
        }
        creds_backup = Some(cb);
    }
    let rm_cb = |cb: &Option<PathBuf>| {
        if let Some(p) = cb {
            rm_f(p);
        }
    };
    // The block holds the access token; it is only ever written, never passed on an argv.
    let merged = match last_json(&live) {
        Some(Value::Object(mut o)) if !live.is_empty() => {
            o.insert("claudeAiOauth".into(), oauth);
            Value::Object(o)
        }
        _ => {
            let mut m = Map::new();
            m.insert("claudeAiOauth".into(), oauth);
            Value::Object(m)
        }
    };
    let merged = serde_json::to_string(&merged).unwrap_or_default();
    // The three live writes are not one transaction. The intent is recorded in
    // the identity lock first, so that whatever cuts them short leaves a fact
    // for the next run to find, and the lock stays until a person has looked.
    if switch_intent::publish(&ctx.store, alias).is_err() {
        eprintln!("error: could not record account switch intent; live identity was not changed");
        rm_cb(&creds_backup);
        cleanup(&backups);
        return Err(1);
    }
    if ctx.creds_write(&merged).is_err() {
        // The backend may have written before it failed, or failed to put its old
        // value back: keep the intent, the lock and the backups rather than guess
        // which login is live.
        if !creds_had {
            rm_cb(&creds_backup);
            creds_backup = None;
        }
        name_kept_copies(creds_backup.iter().chain(&backups));
        return Err(1);
    }
    switch_intent::fault("creds");
    let mut failed = false;
    for (text, d) in staged.iter().zip(&destinations) {
        let point = if *d == ctx.files[0] {
            "primary"
        } else {
            "secondary"
        };
        if write_atomic(d, text.as_bytes(), 0o600).is_err() || switch_intent::fault(point) {
            failed = true;
            break;
        }
    }
    if failed {
        // Every put-back that fails is named, and its backup copy is kept, so
        // that a switch which could not undo itself never looks like one that did.
        let mut not_restored: Vec<String> = Vec::new();
        let mut kept: Vec<PathBuf> = Vec::new();
        for (b, d) in backups.iter().zip(&destinations) {
            if switch_intent::fault("rollback") || copy_p(b, d).is_err() {
                not_restored.push(d.display().to_string());
                kept.push(b.clone());
            }
        }
        if keychain {
            if ctx.creds_write(&prior_creds).is_err() {
                // The Keychain has no backup file, so the old login exists only
                // here in memory: write it where the user can get it back.
                match save_prior_login(ctx, &prior_creds) {
                    Some(saved) => {
                        not_restored.push(format!(
                            "Keychain '{KEYCHAIN_SVC}' (the previous login is saved in {})",
                            saved.display()
                        ));
                        kept.push(saved);
                    }
                    None => not_restored.push(format!("Keychain '{KEYCHAIN_SVC}'")),
                }
            }
        } else if creds_had {
            if let Some(cb) = &creds_backup
                && copy_p(cb, &ctx.creds).is_err()
            {
                not_restored.push(ctx.creds.display().to_string());
                kept.push(cb.clone());
            }
        } else {
            remove_new_creds(&ctx.creds, &mut not_restored);
        }
        if not_restored.is_empty() {
            // Put back in full: the intent can go, and the backups with it, unless
            // the intent cannot be removed, when they stay for inspection.
            if switch_intent::clear(&ctx.store).is_ok() {
                rm_cb(&creds_backup);
                cleanup(&backups);
            } else {
                eprintln!(
                    "error: prior identity restored but switch intent could not be cleared; backups retained for inspection"
                );
                if !creds_had {
                    rm_cb(&creds_backup);
                    creds_backup = None;
                }
                name_kept_copies(creds_backup.iter().chain(&backups));
            }
            eprintln!("error: switch commit failed; restored prior state where possible");
            return Err(1);
        }
        // Nothing is removed that is a kept copy of the previous state.
        if !kept.iter().any(|k| Some(k) == creds_backup.as_ref()) {
            rm_cb(&creds_backup);
        }
        cleanup(
            &backups
                .iter()
                .filter(|b| !kept.contains(b))
                .cloned()
                .collect::<Vec<_>>(),
        );
        eprintln!(
            "error: switch commit failed and the previous login was NOT restored: {} still hold \
             the new login or a partial change",
            not_restored.join(", ")
        );
        name_kept_copies(&kept);
        return Err(1);
    }
    if switch_intent::clear(&ctx.store).is_err() {
        eprintln!("error: account switch completed but its intent could not be cleared");
        // The copies of the previous files stay, as they do when the put-back
        // is what could not be cleared: say where they are. A credentials
        // placeholder for a file that did not exist is not a copy of anything.
        if !creds_had {
            rm_cb(&creds_backup);
            creds_backup = None;
        }
        name_kept_copies(creds_backup.iter().chain(&backups));
        return Err(1);
    }
    rm_cb(&creds_backup);
    for b in &backups {
        rm_f(b);
    }
    lock.done();
    for d in &destinations {
        println!("  patched metadata {}", d.display());
    }
    if ctx.creds.is_file() {
        println!("  patched token    {}", ctx.creds.display());
    } else {
        println!("  patched token    Keychain '{KEYCHAIN_SVC}'");
    }
    Ok(())
}

pub fn cmd_switch(ctx: &Ctx, args: &[String]) -> Res {
    let (alias, force) = alias_force(args);
    check_alias(&alias, false)?;
    alias_exact(&alias, &[&ctx.store])?;
    // Before the plan: "from" would be unknown, and nobody should be asked to
    // confirm a move from a login that cannot be named.
    switch_intent::ready(&ctx.store)?;
    let src = ctx.meta(&alias);
    if !is_file(&src) {
        eprintln!("error: unknown account '{alias}'");
        let names: String = account_aliases(ctx)
            .iter()
            .map(|a| format!("{a} "))
            .collect();
        eprintln!("  registered: {names}");
        return Err(1);
    }
    if !valid_metadata_file(&src) {
        eprintln!("error: malformed metadata snapshot for '{alias}'");
        return Err(1);
    }
    let via_token = ctx.token_oauth_block(&alias).is_some();
    if !via_token {
        let snap = ctx.snap(&alias);
        if !is_file(&snap) || !valid_oauth_file(&snap) {
            eprintln!(
                "error: missing or malformed token snapshot for '{alias}'; refusing a cosmetic switch"
            );
            return Err(1);
        }
        ctx.refuse_expired_login(&alias)?;
    }
    if !is_file(&ctx.files[0]) {
        eprintln!("error: primary live configuration is missing; log in first or use pane");
        return Err(1);
    }
    let from = ctx.current_alias();
    let from_email = ctx.current_email();
    let to_email = email_of(read_last_json(&src).as_ref());
    println!("Switch plan:");
    println!(
        "  from : {} ({})",
        if from.is_empty() { "?" } else { &from },
        if from_email.is_empty() {
            "none"
        } else {
            &from_email
        }
    );
    println!("  to   : {alias} ({to_email})");
    if via_token {
        println!("  token: one-year (no refresh token, no race; running sessions follow)");
    } else {
        println!("  token: login snapshot (real switch)");
    }
    // The ceiling refusal comes before the prompt and nothing in the environment lifts it.
    shared_gate(
        ctx,
        &alias,
        &format!("sharkswap --force {alias}"),
        force,
        false,
    )?;
    if env_or_empty("ASSUME_YES") != "1" {
        if is_tty(0) {
            eprint!("Proceed? [y/N] ");
            let _ = std::io::stderr().flush();
        }
        let mut line = String::new();
        let n = std::io::stdin().lock().read_line(&mut line).unwrap_or(0);
        if n == 0 || !line.ends_with('\n') {
            return Err(1);
        }
        let ans = line.trim_matches([' ', '\t', '\n']);
        if ans != "y" && ans != "Y" {
            println!("cancelled.");
            return Ok(());
        }
    }
    apply_block(ctx, &alias)?;
    println!("Done. Active: {to_email}");
    Ok(())
}

// ---- pane -----------------------------------------------------------------------

#[derive(PartialEq, Clone, Copy)]
enum Snap {
    Missing,
    Malformed,
    Lapsed,
    Ok,
}

pub fn cmd_pane(ctx: &Ctx, args: &[String]) -> Res {
    let (alias, force) = alias_force(args);
    if alias.is_empty() {
        eprintln!("sharkswap: alias: usage: sharkswap pane <alias> [--force]");
        return Err(1);
    }
    check_alias(&alias, false)?;
    let meta_src = ctx.meta(&alias);
    let creds_src = ctx.snap(&alias);
    alias_exact(&alias, &[&ctx.store])?;
    if !is_file(&meta_src) {
        eprintln!("error: unknown account '{alias}'");
        return Err(1);
    }
    if !valid_metadata_file(&meta_src) {
        eprintln!("error: malformed metadata for '{alias}'");
        return Err(1);
    }
    // An `eval "$(cs pane work)"` of a refused alias prints nothing on stdout.
    shared_gate(
        ctx,
        &alias,
        &format!("sharkswap pane --force {alias}"),
        force,
        false,
    )?;
    let via_token = ctx.token_usable(&alias);
    let mut snap = Snap::Missing;
    if is_file(&creds_src) {
        snap = if !valid_oauth_file(&creds_src) {
            Snap::Malformed
        } else if ctx
            .login_expiry_sec(&creds_src)
            .is_some_and(|s| s <= wall_secs())
        {
            Snap::Lapsed
        } else {
            Snap::Ok
        };
    }
    if !via_token {
        match snap {
            Snap::Missing => {
                eprintln!(
                    "error: no token snapshot for '{alias}' \u{2014} pane mode needs a real token."
                );
                eprintln!("       Run '/login' as this account, then 'sharkswap save {alias}',");
                eprintln!("       or store a one-year token with 'sharkswap token set {alias}'.");
                return Err(1);
            }
            Snap::Malformed => {
                eprintln!("error: malformed token snapshot for '{alias}'");
                return Err(1);
            }
            Snap::Lapsed => {
                // Only when this pane would be seeded from the store.
                if !ctx.homes.join(&alias).join(".credentials.json").exists() {
                    ctx.refuse_expired_login(&alias)?;
                }
            }
            Snap::Ok => {}
        }
    } else if snap == Snap::Malformed {
        eprintln!(
            "note: login snapshot for '{alias}' is malformed; the pane runs on the one-year token alone"
        );
    }
    let lock = IdLock::acquire(ctx)?;
    if !valid_metadata_file(&meta_src)
        || (snap != Snap::Missing && snap != Snap::Malformed && !valid_oauth_file(&creds_src))
    {
        eprintln!(
            "error: account snapshot changed or became malformed while waiting for the identity lock"
        );
        return Err(1);
    }
    let dir = ctx.homes.join(&alias);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "mkdir: cannot create directory '{}': {}",
            dir.display(),
            os_msg(&e)
        );
        return Err(1);
    }
    let seed = ctx.home.join(".claude").join(".claude.json");
    let config_dst = dir.join(".claude.json");
    let creds_dst = dir.join(".credentials.json");
    if creds_dst.exists() && !config_dst.exists() {
        eprintln!(
            "error: pane credentials have no identity metadata; refusing to relabel {}",
            dir.display()
        );
        return Err(1);
    }
    let Some(block) = read_last_json(&meta_src) else {
        return Err(1);
    };
    // What is to be written, checked now; written together below.
    let mut config_new: Option<String> = None;
    let mut creds_new: Option<String> = None;
    if config_dst.exists() {
        let cfg = read_last_json(&config_dst);
        let acct = cfg
            .as_ref()
            .and_then(|c| c.get("oauthAccount"))
            .filter(|a| a.is_object());
        let Some(acct) = acct else {
            eprintln!(
                "error: existing pane config is malformed; refusing to overwrite {}",
                config_dst.display()
            );
            return Err(1);
        };
        let pane_email = jq_text(acct.get("emailAddress"));
        let saved_email = email_of(Some(&block));
        if pane_email != saved_email {
            eprintln!(
                "error: existing pane identity mismatch ({pane_email} != {saved_email}); refusing to overwrite {}",
                config_dst.display()
            );
            return Err(1);
        }
    } else {
        let text = if is_file(&seed) {
            match read_text(&seed).ok().and_then(|t| with_account(&t, &block)) {
                Some(t) => t,
                None => return Err(1),
            }
        } else {
            let mut m = Map::new();
            m.insert("oauthAccount".into(), block.clone());
            pretty(&Value::Object(m))
        };
        config_new = Some(text);
    }
    if creds_dst.exists() {
        if !valid_oauth_file(&creds_dst) {
            eprintln!(
                "error: existing pane credentials are malformed; refusing to overwrite {}",
                creds_dst.display()
            );
            return Err(1);
        }
    } else if snap == Snap::Ok {
        let Some(v) = read_last_json(&creds_src) else {
            return Err(1);
        };
        creds_new = Some(pretty(&oauth_only(&v)));
    } else if !via_token {
        // The home's login vanished between the check above and the lock.
        let _ = ctx.refuse_expired_login(&alias);
        return Err(1);
    }
    if let Some(text) = &config_new
        && write_atomic(&config_dst, text.as_bytes(), 0o600).is_err()
    {
        return Err(1);
    }
    if let Some(text) = &creds_new
        && write_atomic(&creds_dst, text.as_bytes(), 0o600).is_err()
    {
        if config_new.is_some() {
            rm_f(&config_dst);
        }
        return Err(1);
    }
    lock.done();
    eprintln!(
        "# pane config for {alias} ({}) ready at {}",
        email_of(Some(&block)),
        dir.display()
    );
    eprintln!("# run this in the target pane, then 'claude':");
    println!(
        "export CLAUDE_CONFIG_DIR={}",
        bash_quote(dir.as_os_str().as_encoded_bytes())
    );
    // The token line asks `token print` for the token when it is evaluated; the
    // token itself never reaches the terminal, the scrollback or the history,
    // and the line never holds it. A plain assignment (not `export X=$(...)`)
    // so a failing `token print` fails the line.
    if via_token {
        let exe = std::env::current_exe()
            .ok()
            .filter(|p| p.is_absolute() && p.is_file())
            .map_or_else(
                || "sharkswap".to_string(),
                |p| bash_quote(p.as_os_str().as_encoded_bytes()),
            );
        eprintln!("# and the one-year token for {alias}, asked of 'token print' at eval time:");
        println!(
            "CLAUDE_CODE_OAUTH_TOKEN=\"$({exe} token print {alias})\" && export CLAUDE_CODE_OAUTH_TOKEN || {{ echo 'sharkswap: could not read the token for {alias}; CLAUDE_CODE_OAUTH_TOKEN is now unset' >&2; unset CLAUDE_CODE_OAUTH_TOKEN; }}"
        );
    } else {
        println!("unset CLAUDE_CODE_OAUTH_TOKEN");
    }
    Ok(())
}

// ---- watch ----------------------------------------------------------------------

/// Re-snapshot the active account's login when it has changed.
fn autosave(ctx: &Ctx) {
    let Ok(lock) = IdLock::acquire(ctx) else {
        return;
    };
    let live = ctx.creds_read();
    if live.is_empty() || !valid_oauth_text(&live) {
        return;
    }
    let alias = ctx.current_alias();
    if alias.is_empty() {
        return;
    }
    let dst = ctx.snap(&alias);
    let token_of = |v: Option<Value>| -> String {
        let t = v.and_then(|v| {
            v.get("claudeAiOauth")
                .and_then(|o| o.get("accessToken"))
                .cloned()
        });
        jq_text(t.as_ref())
    };
    let live_tok = token_of(last_json(&live));
    if is_file(&dst) && live_tok == token_of(read_last_json(&dst)) {
        return;
    }
    // Whether a refresh also extends the login lifetime is the open question
    // behind every re-login: say so when the login clock moves.
    let prev_login = if is_file(&dst) {
        ctx.login_expiry_sec(&dst)
    } else {
        None
    };
    let Some(live_v) = last_json(&live) else {
        return;
    };
    if write_atomic(&dst, pretty(&oauth_only(&live_v)).as_bytes(), 0o600).is_err() {
        return;
    }
    lock.done();
    let new_login = ctx.login_expiry_sec(&dst);
    let now = hms(wall_secs());
    println!(
        "[{now}] re-snapshotted token for {alias} (expiry {}; login {} left)",
        ctx.token_expiry(&dst),
        ctx.login_left_short(&dst)
    );
    if let (Some(p), Some(n)) = (prev_login, new_login)
        && p != n
    {
        println!(
            "[{}] login clock for {alias} moved by {}h: a refresh extends the login",
            hms(wall_secs()),
            (n - p) / 3600
        );
    }
}

/// `sleep(1)`'s argument: a number with an optional s, m, h or d.
fn sleep_for(arg: &str) -> Result<Duration, ()> {
    let (num, mult) = match arg.chars().last() {
        Some('s') => (&arg[..arg.len() - 1], 1.0),
        Some('m') => (&arg[..arg.len() - 1], 60.0),
        Some('h') => (&arg[..arg.len() - 1], 3600.0),
        Some('d') => (&arg[..arg.len() - 1], 86400.0),
        _ => (arg, 1.0),
    };
    let ok = !num.is_empty()
        && num.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && num.matches('.').count() <= 1;
    let secs: f64 = if ok {
        num.parse().map_err(|_| ())?
    } else {
        return Err(());
    };
    Ok(Duration::from_secs_f64((secs * mult).min(1.0e9)))
}

fn nap(arg: &str) -> Res {
    match sleep_for(arg) {
        Ok(d) => {
            std::thread::sleep(d);
            Ok(())
        }
        Err(()) => {
            eprintln!("sleep: invalid time interval '{arg}'");
            eprintln!("Try 'sleep --help' for more information.");
            Err(1)
        }
    }
}

pub fn cmd_watch(ctx: &Ctx, args: &[String]) -> Res {
    let target = match args.first() {
        Some(t) if !t.is_empty() => PathBuf::from(t),
        _ => ctx.creds.clone(),
    };
    let email = ctx.current_email();
    if !email.is_empty() && ctx.current_alias().is_empty() {
        eprintln!(
            "WARNING: active account {email} is not registered here \u{2014} nothing will be snapshotted until 'sharkswap save <alias>' is run."
        );
    }
    let interval = |default: &str| -> String {
        let v = env_or_empty("WATCH_INTERVAL");
        if v.is_empty() { default.to_string() } else { v }
    };
    if ctx.darwin && !is_file(&target) {
        let every = interval("30");
        println!(
            "watching Keychain '{KEYCHAIN_SVC}' (poll {every}s) \u{2014} auto-saving active account token on change (Ctrl-C to stop)"
        );
        loop {
            autosave(ctx);
            nap(&every)?;
        }
    }
    println!(
        "watching {} \u{2014} auto-saving active account token on change (Ctrl-C to stop)",
        target.display()
    );
    // Change is judged by content (up to the first NUL), not by mtime: a rewrite
    // of the same bytes runs nothing. The first pass always runs.
    let every = interval("10");
    let mut last: Option<Vec<u8>> = None;
    loop {
        let mut cur = read_bytes(&target).unwrap_or_default();
        if let Some(i) = cur.iter().position(|&b| b == 0) {
            cur.truncate(i);
        }
        if last.as_ref() != Some(&cur) {
            autosave(ctx);
            last = Some(cur);
        }
        nap(&every)?;
    }
}

// ---- one-year tokens ----------------------------------------------------------------

fn setup_token_ok(t: &str) -> bool {
    let Some(rest) = t.strip_prefix("sk-ant-oat") else {
        return false;
    };
    let b = rest.as_bytes();
    if b.len() < 3 || !b[0].is_ascii_digit() || !b[1].is_ascii_digit() || b[2] != b'-' {
        return false;
    }
    let tail = &b[3..];
    tail.len() >= 40
        && tail
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
}

/// The terminal attributes to put back if a signal ends the process while the
/// typing is not echoed. Set before the handler is installed.
static ECHO_TERMIOS: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();

/// A hangup, Ctrl-C, Ctrl-\ or SIGTERM while a secret is being typed: echo back on, then
/// the signal takes its default effect, so the shell sees the usual status and
/// the terminal is not left silent.
extern "C" fn echo_on_signal(sig: libc::c_int) {
    if let Some(t) = ECHO_TERMIOS.get() {
        // SAFETY: tcsetattr is async-signal-safe and `t` is a valid termios.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
    }
    // SAFETY: signal and raise are async-signal-safe. The signal is blocked while
    // this handler runs, so the default action follows when the handler returns.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Echo is off from `new` to the drop, and is put back on a normal return, an
/// early return or a panic, and (through `echo_on_signal`) on a signal.
struct EchoOff {
    saved: libc::termios,
    signals: SavedSignals,
}

impl EchoOff {
    fn new() -> Option<EchoOff> {
        // SAFETY: tcgetattr/tcsetattr work on a plain struct and descriptor 0.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return None;
            }
            let _ = ECHO_TERMIOS.set(t);
            let signals = catch_signals(echo_on_signal);
            let mut quiet = t;
            quiet.c_lflag &= !libc::ECHO;
            libc::tcsetattr(0, libc::TCSANOW, &quiet);
            Some(EchoOff { saved: t, signals })
        }
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        // SAFETY: restores the attributes read in `new`.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.saved) };
        self.signals.restore();
    }
}

/// One line from stdin; with a terminal, typing is not echoed, and the echo
/// comes back however the read ends: a line, end of input, Ctrl-C, Ctrl-\,
/// SIGTERM or SIGHUP. (Ctrl-Z, which only pauses the job, is not handled.)
fn read_secret_line() -> String {
    let quiet = if is_tty(0) { EchoOff::new() } else { None };
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    if quiet.is_some() {
        drop(quiet);
        eprintln!();
    }
    line.trim_end_matches(['\n', '\r']).to_string()
}

/// Whether the API places `token` in the organization `alias` was saved with
/// (`organizationUuid` in its metadata). A token made while another account was
/// logged in would otherwise be filed under `alias`, and its probes would show
/// that account's usage there. A different organization is refused (exit 75, like
/// the other refusals `--force` lifts) unless `force`. When the two cannot be
/// compared (the metadata names no valid organization id, the API did not answer
/// or sent none, as it may for a 401 or 403) the token is still stored, with a
/// note.
fn check_token_org(ctx: &Ctx, alias: &str, token: &str, force: bool) -> Res {
    let saved = read_json(&ctx.meta(alias))
        .and_then(|v| v.get("organizationUuid").cloned())
        .filter(|v| !v.is_null());
    let Some(want) = saved.as_ref().and_then(|v| probe::org_id(v.as_str()?)) else {
        let what = if saved.is_some() {
            "no valid organization id"
        } else {
            "no organization"
        };
        eprintln!(
            "note: the organization of this token was not checked: the login saved as '{alias}' names {what}"
        );
        return Ok(());
    };
    let reply = probe::ask(token);
    let said = reply
        .message
        .as_deref()
        .map_or(String::new(), |m| format!(": {m}"));
    let Some(got) = reply.org else {
        match reply.http {
            0 => eprintln!(
                "note: the organization of this token was not checked: the API did not answer"
            ),
            401 => eprintln!(
                "note: the API refused this token with 401{said}; its organization was not checked and the token is stored anyway. Check that the whole token was pasted."
            ),
            403 => eprintln!(
                "note: the API refused this token with 403{said}; its organization was not checked."
            ),
            http => eprintln!(
                "note: the organization of this token was not checked: the API answered {http} without an organization id"
            ),
        }
        return Ok(());
    };
    if got == want {
        if reply.http == 403 {
            eprintln!(
                "note: the API answered 403{said}; the token is in the saved organization, but the account is refused now."
            );
        }
        return Ok(());
    }
    if force {
        eprintln!(
            "override: --force stored the token for '{alias}' although the API places it in organization {got}, not {want}"
        );
        return Ok(());
    }
    eprintln!(
        "error: this token belongs to organization {got}, but '{alias}' was saved with organization {want}; nothing stored"
    );
    eprintln!(
        "  Either it was made while another account was logged in: log in to '{alias}', run 'claude setup-token' again and set that token."
    );
    eprintln!(
        "  Or the organization of '{alias}' changed since 'sharkswap save' (for example it moved to a team plan): log in to it and run 'sharkswap save {alias}' again."
    );
    eprintln!(
        "  To store it anyway: sharkswap token set --force {alias}   (the token is read from stdin again, as for any 'token set')"
    );
    Err(75)
}

pub fn cmd_token(ctx: &Ctx, args: &[String]) -> Res {
    let sub = args.first().map_or("", String::as_str);
    let rest = if args.is_empty() { args } else { &args[1..] };
    match sub {
        "set" => {
            let (alias, force) = alias_force(rest);
            if alias.is_empty() {
                eprintln!(
                    "sharkswap: 1: usage: sharkswap token set [--force] <alias>   (token on stdin)"
                );
                return Err(1);
            }
            check_alias(&alias, false)?;
            alias_exact(&alias, &[&ctx.store, &ctx.tokens])?;
            if !is_file(&ctx.meta(&alias)) {
                eprintln!("error: unknown alias '{alias}' (save its metadata first)");
                return Err(1);
            }
            if is_tty(0) {
                eprintln!(
                    "paste the token from 'claude setup-token', then Enter (it is not echoed):"
                );
            }
            let token: String = read_secret_line()
                .chars()
                .filter(|c| !matches!(c, '\r' | '\n' | '\t' | ' '))
                .collect();
            if !setup_token_ok(&token) {
                eprintln!(
                    "error: that is not a setup-token (expected sk-ant-oatNN-... as printed by claude setup-token)"
                );
                return Err(1);
            }
            check_token_org(ctx, &alias, &token, force)?;
            if std::fs::create_dir_all(&ctx.tokens).is_err()
                || std::fs::set_permissions(
                    &ctx.tokens,
                    std::os::unix::fs::PermissionsExt::from_mode(0o700),
                )
                .is_err()
            {
                return Err(1);
            }
            let mut m = Map::new();
            m.insert("token".into(), Value::String(token));
            m.insert("created".into(), Value::from(wall_secs()));
            let dst = ctx.token_file(&alias);
            if write_atomic(&dst, pretty(&Value::Object(m)).as_bytes(), 0o600).is_err() {
                eprintln!("error: could not store the token");
                return Err(1);
            }
            println!(
                "stored one-year token for {alias} in {} (switch and pane use it from now on)",
                dst.display()
            );
            Ok(())
        }
        "print" => {
            let alias = rest.first().cloned().unwrap_or_default();
            if alias.is_empty() {
                eprintln!("sharkswap: 1: usage: sharkswap token print <alias>");
                return Err(1);
            }
            check_alias(&alias, false)?;
            // The one place a token is written to stdout, for `$(...)` in the line
            // `pane` prints. On a terminal it would land in the scrollback.
            if is_tty(1) {
                eprintln!(
                    "error: refusing to print a token to a terminal; it is for $(...) or a pipe, e.g. the line 'sharkswap pane {alias}' prints"
                );
                return Err(1);
            }
            if !ctx.token_usable(&alias) {
                eprintln!("error: no usable one-year token stored for {alias}");
                return Err(1);
            }
            let v = read_last_json(&ctx.token_file(&alias));
            let Some(token) = token_text(v.as_ref()) else {
                eprintln!("error: token file unreadable");
                return Err(1);
            };
            let mut out = std::io::stdout().lock();
            if writeln!(out, "{token}").and_then(|()| out.flush()).is_err() {
                return Err(1);
            }
            Ok(())
        }
        "rm" | "remove" => {
            let alias = rest.first().cloned().unwrap_or_default();
            if alias.is_empty() {
                eprintln!("sharkswap: 1: usage: sharkswap token rm <alias>");
                return Err(1);
            }
            check_alias(&alias, false)?;
            rm_f(&ctx.token_file(&alias));
            println!("removed token for {alias}");
            Ok(())
        }
        "list" | "ls" | "" => {
            let mut any = false;
            for f in glob_json(&ctx.tokens) {
                any = true;
                let name = &f[..f.len() - ".json".len()];
                println!(
                    "  {} {} left",
                    pad_r(name, 12),
                    ctx.token_left_short(&ctx.tokens.join(&f))
                );
            }
            if !any {
                println!(
                    "no one-year tokens stored (claude setup-token, then: sharkswap token set <alias>)"
                );
            }
            Ok(())
        }
        "check" => {
            let alias = rest.first().cloned().unwrap_or_default();
            if alias.is_empty() {
                eprintln!("sharkswap: 1: usage: sharkswap token check <alias> [--model]");
                return Err(1);
            }
            check_alias(&alias, false)?;
            let f = ctx.token_file(&alias);
            if !is_file(&f) {
                eprintln!("error: no token stored for {alias}");
                return Err(1);
            }
            let tok = jq_text(read_last_json(&f).as_ref().and_then(|v| v.get("token")));
            if tok.is_empty() {
                eprintln!("error: token file unreadable");
                return Err(1);
            }
            let version = Command::new("claude")
                .arg("--version")
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_string()
                })
                .unwrap_or_default();
            let code = probe::usage_code(&tok, &format!("claude-code/{version}"));
            match code.as_str() {
                "200" => println!("usage endpoint: 200, quota can be read through this token"),
                "401" | "403" => println!(
                    "usage endpoint: {code}, token lacks the profile scope; use 'sharkswap probe' to read usage through it"
                ),
                _ => println!("usage endpoint: http {code}"),
            }
            if rest.get(1).map(String::as_str) == Some("--model") {
                // A throwaway config dir: the check neither reads nor writes the real ~/.claude.
                let tmpdir = std::env::var("TMPDIR")
                    .ok()
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| "/tmp".into());
                let Ok(cfg) = mktemp_dir(format!("{tmpdir}/sharkswap-token-check.")) else {
                    eprintln!("error: could not make a temp dir for the model check");
                    return Err(1);
                };
                let out = Command::new("claude")
                    .args(["-p", "reply with the single word OK", "--model", "haiku"])
                    .env("CLAUDE_CODE_OAUTH_TOKEN", &tok)
                    .env("CLAUDE_CONFIG_DIR", &cfg)
                    .stdin(Stdio::null())
                    .output();
                let _ = std::fs::remove_dir_all(&cfg);
                let mut text = String::new();
                if let Ok(o) = out {
                    text.push_str(&String::from_utf8_lossy(&o.stdout));
                    text.push_str(&String::from_utf8_lossy(&o.stderr));
                }
                let reply = text
                    .strip_suffix('\n')
                    .unwrap_or(&text)
                    .rsplit('\n')
                    .next()
                    .unwrap_or("")
                    .to_string();
                let reply = trim_nl(&reply);
                if reply.contains("OK") {
                    println!("model request: ok");
                } else {
                    println!(
                        "model request: failed ({})",
                        reply.chars().take(80).collect::<String>()
                    );
                }
            }
            Ok(())
        }
        _ => {
            eprintln!("usage: sharkswap token set|print|rm|list|check <alias> ...");
            Err(1)
        }
    }
}

// ---- best, bar, overview, next ----------------------------------------------------------

pub fn cmd_best(ctx: &Ctx, args: &[String]) -> Res {
    let (mut do_switch, mut do_raw) = (false, false);
    for a in args {
        match a.as_str() {
            "--switch" => do_switch = true,
            "--raw" => do_raw = true,
            _ => {
                eprintln!("error: usage: best [--switch|--raw]");
                return Err(1);
            }
        }
    }
    if do_switch && do_raw {
        eprintln!("error: --raw prints the ranked rows; it does not switch");
        return Err(1);
    }
    // Err(3): the ranking refused and has said why.
    let Ok(rows) = crate::views::best(&ctx.store) else {
        return Err(1);
    };
    let picks = trim_nl(&rows);
    if picks.is_empty() {
        eprintln!("no saved account has a usage reading or a probe answer");
        eprintln!("  log in to each account, run 'sharkswap save <alias>', then 'sharkswap probe'");
        return Err(1);
    }
    if do_raw {
        println!("{picks}");
        return Ok(());
    }
    let fields = |l: &str| l.split('\t').map(str::to_string).collect::<Vec<String>>();
    if do_switch {
        // `fresh` and `PROBE-OK` are the candidates; picks is ordered most usable first.
        let top = picks
            .lines()
            .map(fields)
            .find(|f| matches!(f.get(4).map(String::as_str), Some("fresh" | "PROBE-OK")));
        let Some(top) = top else {
            eprintln!(
                "no usable saved account (all stale, expired, spent, 5h-blocked or refused): re-login an alias, run 'sharkswap probe', or wait for a window"
            );
            return Err(1);
        };
        let name = top[0].clone();
        let state = top[4].clone();
        match state.as_str() {
            "fresh" => {
                println!("switching to {name} (least of its binding window consumed, usable now)")
            }
            "PROBE-OK" => println!(
                "switching to {name} (no readable usage window; it answered a probe just now)"
            ),
            _ => println!("switching to {name} ({state})"),
        }
        return cmd_switch(ctx, &[name]);
    }
    println!(
        "Accounts, most usable first (ranked by max(WK, 5H); blocked and dead snapshots last):"
    );
    println!(
        "  Percentages come from 'sharkswap probe' (rate-limit headers), a few points above the usage page."
    );
    println!(
        "  PROBE-OK = answered, no windows read. PROBE-? = never probed or the reading is old. PROBE-ERR = token refused."
    );
    println!(
        "  BLOCKED = 5h window spent. SPENT = week spent or refused. STALE/EXPIRED = saved login too old or lapsed."
    );
    println!(
        "  {:<12} {:>5} {:>5}  {:<26} {:>5}  STATE",
        "ALIAS", "WK", "5H", "RESETS AT", "AGE"
    );
    let cell = |v: &str| -> String {
        let digits = v.strip_prefix('-').unwrap_or(v);
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            format!(
                "{:>4}%",
                v.parse::<i64>().map_or(v.to_string(), |n| n.to_string())
            )
        } else {
            format!("{v:>5}")
        }
    };
    for l in picks.lines() {
        let f = fields(l);
        let at = |i: usize| f.get(i).map_or("", String::as_str);
        let age = if at(3) == "-" {
            "    -".to_string()
        } else {
            format!("{:4.1}d", at(3).parse::<f64>().unwrap_or(0.0))
        };
        println!(
            "  {:<12} {} {}  {:<26} {}  {}",
            at(0),
            cell(at(1)),
            cell(at(5)),
            at(2),
            age,
            at(4)
        );
    }
    Ok(())
}

pub fn cmd_bar(ctx: &Ctx, args: &[String]) -> Res {
    let mut alias = args.first().cloned().unwrap_or_default();
    if alias.is_empty() {
        let cfg = env_or_empty("CLAUDE_CONFIG_DIR");
        let prefix = format!("{}/", ctx.homes.display());
        if let Some(rest) = cfg.strip_prefix(&prefix) {
            alias = rest.split('/').next().unwrap_or("").to_string();
        } else {
            alias = ctx.current_alias();
        }
    }
    if alias.is_empty() {
        println!("sharkswap: no account");
        return Ok(());
    }
    check_alias(&alias, false)?;
    println!("{}", crate::views::bar(&ctx.store, &alias));
    Ok(())
}

pub fn cmd_overview(ctx: &Ctx) -> Res {
    cmd_status(ctx)?;
    println!();
    let _ = cmd_best(ctx, &[]);
    Ok(())
}

pub fn cmd_toggle(ctx: &Ctx) -> Res {
    switch_intent::ready(&ctx.store)?;
    let files = account_aliases(ctx);
    if files.is_empty() {
        eprintln!("error: no accounts registered in {}", ctx.store.display());
        return Err(1);
    }
    let active = ctx.current_alias();
    let start = if active.is_empty() {
        0
    } else {
        match files.iter().position(|a| *a == active) {
            Some(i) => (i + 1) % files.len(),
            None => {
                eprintln!("error: couldn't determine next account");
                return Err(1);
            }
        }
    };
    // The first account in round-robin order the ceiling gate lets through; the
    // last candidate is the active account itself.
    for n in 0..files.len() {
        let next = &files[(start + n) % files.len()];
        if shared_gate(ctx, next, &format!("sharkswap --force {next}"), false, true).is_ok() {
            return cmd_switch(ctx, std::slice::from_ref(next));
        }
        eprintln!("note: skipping '{next}': the ceiling gate refuses it (see: sharkswap {next})");
    }
    eprintln!("error: no account to switch to: the ceiling gate refuses every one");
    Err(75)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestStore;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn a_creds_file_made_by_the_switch_is_removed_on_rollback() {
        let dir = TestStore::new(&[("creds", Value::String("{}".into()))]);
        let mut not_restored = Vec::new();
        remove_new_creds(&dir.join("creds"), &mut not_restored);
        assert!(not_restored.is_empty());
        assert!(!dir.join("creds").exists());
    }

    /// `rm_f` ignores its error; a path that is still there afterwards (a
    /// directory cannot be unlinked as a file) must be reported, not forgotten.
    #[test]
    fn a_creds_path_that_cannot_be_removed_is_reported() {
        let dir = TestStore::new(&[("creds/inner", Value::String("{}".into()))]);
        let creds = dir.join("creds");
        let mut not_restored = vec!["earlier".to_string()];
        remove_new_creds(&creds, &mut not_restored);
        assert_eq!(
            not_restored,
            ["earlier".to_string(), creds.display().to_string()]
        );
        remove_new_creds(&dir.join("never-there"), &mut not_restored);
        assert_eq!(not_restored.len(), 2, "a path that is gone adds nothing");
    }

    #[test]
    fn the_prior_login_is_saved_privately_in_the_store() {
        let home = TestStore::new(&[]);
        let ctx = Ctx::at(home.to_path_buf(), true).unwrap_or_else(|_| panic!("a context"));
        let prior = r#"{"claudeAiOauth":{"accessToken":"old"}}"#;
        let one = save_prior_login(&ctx, prior).expect("saved");
        let two = save_prior_login(&ctx, prior).expect("saved again");
        assert_ne!(one, two, "a second save never overwrites the first");
        assert_eq!(one.parent(), Some(ctx.store.as_path()));
        assert_eq!(
            std::fs::read_to_string(&one).expect("read"),
            format!("{prior}\n")
        );
        assert_eq!(std::fs::metadata(&one).expect("stat").mode() & 0o777, 0o600);
    }

    #[test]
    fn a_prior_login_that_cannot_be_saved_is_none() {
        let home = TestStore::new(&[]);
        let ctx = Ctx::at(home.to_path_buf(), true).unwrap_or_else(|_| panic!("a context"));
        std::fs::remove_dir_all(&ctx.store).expect("rmdir");
        assert_eq!(save_prior_login(&ctx, "{}"), None);
    }
}
