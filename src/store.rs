//! Where things live, how the live login is read and written, the identity
//! lock, alias rules, and the expiry texts.

use crate::util::*;
use serde_json::{Map, Value};
use std::fs::Permissions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

pub const KEYCHAIN_SVC: &str = "Claude Code-credentials";
pub const DAY: i64 = 86_400;

/// Every path the tool uses, resolved once from `$HOME`.
pub struct Ctx {
    pub home: PathBuf,
    pub store: PathBuf,
    pub homes: PathBuf,
    pub tokens: PathBuf,
    pub files: [PathBuf; 2],
    pub creds: PathBuf,
    pub darwin: bool,
}

impl Ctx {
    /// The context for `$HOME`. `warn` says whether a store that is too open may
    /// be reported on stderr: a status line or a background watcher must not.
    pub fn new(warn: bool) -> Result<Ctx, i32> {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            eprintln!("sharkswap: HOME: unbound variable");
            return Err(1);
        };
        Ctx::at(home, warn)
    }

    /// The context for the home directory `home`, with its store made (mode 0700).
    pub fn at(home: PathBuf, warn: bool) -> Result<Ctx, i32> {
        let store = home.join(".sharkswap");
        let ctx = Ctx {
            homes: store.join("homes"),
            tokens: store.join("tokens"),
            files: [
                home.join(".claude.json"),
                home.join(".claude").join(".claude.json"),
            ],
            creds: home.join(".claude").join(".credentials.json"),
            darwin: on_mac(),
            store,
            home,
        };
        if !ctx.store.is_dir()
            && let Err(e) = std::fs::create_dir_all(&ctx.store)
        {
            eprintln!(
                "mkdir: cannot create directory '{}': {}",
                ctx.store.display(),
                os_msg(&e)
            );
            return Err(1);
        }
        // The store holds login tokens: a directory made loose by an older
        // version or by hand is tightened, and said so if it cannot be. A
        // symbolic link is never followed to change what it points at.
        let mode = std::fs::metadata(&ctx.store).map(|m| m.permissions().mode());
        if mode.is_ok_and(|m| m & 0o077 != 0) {
            let link =
                std::fs::symlink_metadata(&ctx.store).is_ok_and(|m| m.file_type().is_symlink());
            if link {
                if warn {
                    eprintln!(
                        "warning: {} is a symbolic link to a directory readable by other users; \
                         its mode is left as it is (run chmod 700 on the directory it points to)",
                        ctx.store.display()
                    );
                }
            } else if let Err(e) =
                std::fs::set_permissions(&ctx.store, Permissions::from_mode(0o700))
                && warn
            {
                eprintln!(
                    "warning: {} is readable by other users and cannot be set to 0700: {}",
                    ctx.store.display(),
                    os_msg(&e)
                );
            }
        }
        Ok(ctx)
    }

    /// `<store>/<alias>.json`
    pub fn meta(&self, alias: &str) -> PathBuf {
        self.store.join(format!("{alias}.json"))
    }

    /// `<store>/<alias>.creds.json`
    pub fn snap(&self, alias: &str) -> PathBuf {
        self.store.join(format!("{alias}.creds.json"))
    }

    pub fn token_file(&self, alias: &str) -> PathBuf {
        self.tokens.join(format!("{alias}.json"))
    }

    // ---- the live login ---------------------------------------------------

    /// The live credentials text: the file when present, the Keychain on macOS
    /// otherwise, empty when there is none (or it is locked).
    pub fn creds_read(&self) -> String {
        if self.creds.is_file() {
            return read_text(&self.creds)
                .map(|t| trim_nl(&t).to_string())
                .unwrap_or_default();
        }
        if self.darwin {
            return keychain_read().unwrap_or_default();
        }
        String::new()
    }

    /// Write the full credentials JSON: Keychain on a Mac with no file, else an
    /// atomic file replace with a rollback copy. Prints its own errors.
    pub fn creds_write(&self, json: &str) -> Result<(), i32> {
        if self.darwin && !self.creds.is_file() {
            return keychain_write(json);
        }
        self.creds_file_write(json).map_err(|why| {
            eprintln!("error: {why}");
            1
        })
    }

    /// The file form of `creds_write`; the error is the line to print.
    fn creds_file_write(&self, json: &str) -> Result<(), String> {
        if let Some(dir) = self.creds.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create {}: {}", dir.display(), os_msg(&e)))?;
        }
        let backup = mktemp(suffixed(&self.creds, ".bak.")).map_err(|e| {
            format!(
                "cannot make a rollback copy of {}: {}",
                self.creds.display(),
                os_msg(&e)
            )
        })?;
        let mut had = false;
        if self.creds.exists() {
            had = true;
            if let Err(e) = copy_p(&self.creds, &backup) {
                rm_f(&backup);
                return Err(format!(
                    "cannot copy {} to {}: {}",
                    self.creds.display(),
                    backup.display(),
                    os_msg(&e)
                ));
            }
        }
        if let Err(e) = write_atomic(&self.creds, format!("{json}\n").as_bytes(), 0o600) {
            if had {
                let _ = copy_p(&backup, &self.creds);
            } else {
                rm_f(&self.creds);
            }
            rm_f(&backup);
            return Err(format!(
                "credentials write to {} failed: {}; restored prior state where possible",
                self.creds.display(),
                os_msg(&e)
            ));
        }
        rm_f(&backup);
        Ok(())
    }

    /// The live email, empty when there is none or when an unfinished switch makes
    /// the metadata untrustworthy (the check comes before the file is read).
    pub fn current_email(&self) -> String {
        if crate::switch_intent::pending(&self.store) {
            return String::new();
        }
        let Some(v) = read_json(&self.files[0]) else {
            return String::new();
        };
        match v.get("oauthAccount") {
            Some(Value::Object(o)) => jq_text(o.get("emailAddress")),
            _ => String::new(),
        }
    }

    /// The saved alias whose metadata holds the live email, first by name.
    pub fn current_alias(&self) -> String {
        let email = self.current_email();
        if email.is_empty() {
            return String::new();
        }
        for f in glob_json(&self.store) {
            if f.ends_with(".creds.json") || crate::avail::is_own_file(&f) {
                continue;
            }
            let Some(v) = read_json(&self.store.join(&f)) else {
                continue;
            };
            let Value::Object(o) = &v else { continue };
            if jq_text(o.get("emailAddress")) == email {
                // A switch may have started during the scan.
                if crate::switch_intent::pending(&self.store) {
                    return String::new();
                }
                return f[..f.len() - 5].to_string();
            }
        }
        String::new()
    }

    // ---- expiry texts -------------------------------------------------------

    /// "2030-01-15 11:01", plus the EXPIRED note once the access token lapsed.
    pub fn token_expiry(&self, src: &Path) -> String {
        token_expiry_of(read_last_json(src).as_ref())
    }

    /// Epoch seconds when the saved login lapses; None for snapshots without the field.
    pub fn login_expiry_sec(&self, src: &Path) -> Option<i64> {
        login_sec_of(read_last_json(src).as_ref())
    }

    pub fn login_expiry(&self, src: &Path) -> String {
        login_expiry_of(read_last_json(src).as_ref())
    }

    /// "28d", "5h", "EXPIRED" or "?".
    pub fn login_left_short(&self, src: &Path) -> String {
        let Some(sec) = self.login_expiry_sec(src) else {
            return "?".into();
        };
        let left = sec - wall_secs();
        if left <= 0 {
            "EXPIRED".into()
        } else if left < DAY {
            format!("{}h", (left + 1800) / 3600)
        } else {
            format!("{}d", left / DAY)
        }
    }

    /// A saved login that has lapsed cannot be launched; one clear line says so.
    pub fn refuse_expired_login(&self, alias: &str) -> Result<(), i32> {
        let file = self.snap(alias);
        let Some(sec) = self.login_expiry_sec(&file) else {
            return Ok(());
        };
        if sec <= wall_secs() {
            let text = self.login_expiry(&file);
            let head: Vec<&str> = text.split(' ').take(2).collect();
            eprintln!(
                "error: saved login for '{alias}' expired {}; /login as that account, then 'sharkswap save {alias}'",
                head.join(" ")
            );
            return Err(1);
        }
        Ok(())
    }

    // ---- one-year tokens ----------------------------------------------------

    /// True when the alias has a one-year token that can still authenticate.
    /// A missing file is silent; any other refusal says why.
    pub fn token_usable(&self, alias: &str) -> bool {
        self.token_check(alias, false)
    }

    /// `token_usable`, saying nothing when `quiet`.
    pub fn token_check(&self, alias: &str, quiet: bool) -> bool {
        let f = self.token_file(alias);
        if !f.exists() {
            return false;
        }
        let v = read_last_json(&f);
        if token_text(v.as_ref()).is_none() {
            if !quiet {
                eprintln!(
                    "warning: token file for '{alias}' holds no token; ignoring {}",
                    f.display()
                );
            }
            return false;
        }
        if let Some(created) = created_of(v.as_ref())
            && created + 365 * DAY <= wall_secs()
        {
            if !quiet {
                eprintln!(
                    "warning: one-year token for '{alias}' has expired; ignoring {} (renew: sharkswap token set {alias})",
                    f.display()
                );
            }
            return false;
        }
        true
    }

    /// Days of the year left, "EXPIRED", or "?" when the creation time is unknown.
    pub fn token_left_short(&self, f: &Path) -> String {
        let Some(created) = created_of(read_last_json(f).as_ref()) else {
            return "?".into();
        };
        let left = created + 365 * DAY - wall_secs();
        if left <= 0 {
            "EXPIRED".into()
        } else {
            format!("{}d", left / DAY)
        }
    }

    /// A one-year token as the credentials block `claude` reads. None when the
    /// alias has no usable token. The year is checked again here.
    pub fn token_oauth_block(&self, alias: &str) -> Option<Value> {
        let f = self.token_file(alias);
        if !f.is_file() {
            return None;
        }
        let v = read_last_json(&f);
        let token = token_text(v.as_ref())?.to_string();
        let created = created_of(v.as_ref())?;
        if created + 365 * DAY <= wall_secs() {
            return None;
        }
        let mut sub = Value::String("max".into());
        let mut tier = Value::Null;
        let snap = self.snap(alias);
        if snap.is_file() {
            let s = read_json(&snap)?;
            let inner = match &s {
                Value::Object(o) => o.get("claudeAiOauth"),
                Value::Null => None,
                _ => return None,
            };
            let obj = match inner {
                None | Some(Value::Null) => None,
                Some(Value::Object(o)) => Some(o),
                Some(_) => return None,
            };
            if let Some(o) = obj {
                let truthy = |x: &&Value| !matches!(x, Value::Null | Value::Bool(false));
                if let Some(x) = o.get("subscriptionType").filter(truthy) {
                    sub = Value::String(jq_text(Some(x)));
                }
                if let Some(x) = o.get("rateLimitTier").filter(truthy) {
                    tier = x.clone();
                }
            }
        }
        let exp = json_number(((created + 364 * DAY) * 1000) as f64);
        let mut m = Map::new();
        m.insert("accessToken".into(), Value::String(token));
        m.insert("refreshToken".into(), Value::String(String::new()));
        m.insert("expiresAt".into(), exp.clone());
        m.insert("refreshTokenExpiresAt".into(), exp);
        m.insert(
            "scopes".into(),
            Value::Array(vec![Value::String("user:inference".into())]),
        );
        m.insert("subscriptionType".into(), sub);
        if !tier.is_null() {
            m.insert("rateLimitTier".into(), tier);
        }
        Some(Value::Object(m))
    }
}

/// `token_expiry` over an already parsed credentials value.
pub fn token_expiry_of(v: Option<&Value>) -> String {
    let Some(ms) = v
        .and_then(|v| dual_field(v, "expiresAt"))
        .and_then(num)
        .filter(|n| safe_whole(*n))
    else {
        return "?".into();
    };
    let sec = (ms / 1000.0).trunc() as i64;
    let when = local_fmt(sec, "%Y-%m-%d %H:%M").unwrap_or_else(|| "?".into());
    if sec < wall_secs() {
        format!("{when} (EXPIRED \u{2014} will auto-refresh via refreshToken)")
    } else {
        when
    }
}

/// Epoch seconds when the login lapses, from a parsed credentials value.
pub fn login_sec_of(v: Option<&Value>) -> Option<i64> {
    let ms = dual_field(v?, "refreshTokenExpiresAt")
        .and_then(num)
        .filter(|n| *n > 0.0 && safe_whole(*n))?;
    Some((ms / 1000.0).floor() as i64)
}

/// `login_expiry` over an already parsed credentials value.
pub fn login_expiry_of(v: Option<&Value>) -> String {
    let Some(sec) = login_sec_of(v) else {
        return "unknown (snapshot predates the field)".into();
    };
    let when = local_fmt(sec, "%Y-%m-%d %H:%M").unwrap_or_else(|| "?".into());
    let left = sec - wall_secs();
    if left <= 0 {
        format!("{when} (EXPIRED \u{2014} /login needed)")
    } else if left < DAY {
        format!(
            "{when} ({}h left \u{2014} /login soon)",
            (left + 1800) / 3600
        )
    } else if left < 3 * DAY {
        format!("{when} ({}d left \u{2014} /login soon)", left / DAY)
    } else {
        format!("{when} ({}d left)", left / DAY)
    }
}

/// `(.claudeAiOauth.KEY // .KEY)`: the nested field, else the top-level one.
/// Anything jq would refuse to index gives None.
pub fn dual_field<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    let Value::Object(obj) = v else { return None };
    let truthy = |x: &&Value| !matches!(x, Value::Null | Value::Bool(false));
    let inner = match obj.get("claudeAiOauth") {
        None | Some(Value::Null) => None,
        Some(Value::Object(o)) => o.get(key),
        Some(_) => return None,
    };
    inner.filter(truthy).or_else(|| obj.get(key).filter(truthy))
}

/// The token of a `tokens/<alias>.json` value: a non-empty string.
pub fn token_text(v: Option<&Value>) -> Option<&str> {
    v?.as_object()?
        .get("token")?
        .as_str()
        .filter(|s| !s.is_empty())
}

/// The recorded creation time: a number in [0, 1e11), floored.
pub fn created_of(v: Option<&Value>) -> Option<i64> {
    let n = num(v?.as_object()?.get("created")?)?;
    if (0.0..1e11).contains(&n) {
        Some(n.floor() as i64)
    } else {
        None
    }
}

// ---- the Keychain -------------------------------------------------------------

/// The system `security` tool, by absolute path: a program of that name ahead
/// of it in PATH must never be handed the credentials. Only a test hook can
/// name another.
const SECURITY: &str = "/usr/bin/security";

// Test hooks. They exist only in the unit tests and in a build with the
// `test-hooks` feature; any other build has no code that reads these variables.
// `SHARKSWAP_TEST_HOOKS=1` switches them on, and then:
//   SHARKSWAP_TEST_KEYCHAIN  a keychain file to use instead of the search list
//   SHARKSWAP_TEST_SERVICE   another item name than `Claude Code-credentials`
//   SHARKSWAP_TEST_SECURITY  another `security` program (a stub)
//   SHARKSWAP_TEST_DARWIN=1  take the macOS code path on any system
//   SHARKSWAP_NOW            the instant (epoch seconds) every judgement is made at
//   SHARKSWAP_LOCK_ATTEMPTS  how many times the identity lock is tried (default 50)
//   SHARKSWAP_LOCK_DELAY     seconds between those tries (default 0.1)
//   SHARKSWAP_TEST_SWITCH_FAULT  where a switch is interrupted or made to fail (switch_intent.rs)
// They let CI round-trip a login through a throwaway keychain, let the
// black-box tests run the Keychain path against a stub on Linux, and let a test
// pin the clock or shorten the lock wait. They are not settings, and nothing
// outside the tests sets them.
#[cfg(any(test, feature = "test-hooks"))]
const HOOKS: &str = "SHARKSWAP_TEST_HOOKS";
#[cfg(any(test, feature = "test-hooks"))]
const HOOK_KEYCHAIN: &str = "SHARKSWAP_TEST_KEYCHAIN";
#[cfg(any(test, feature = "test-hooks"))]
const HOOK_SERVICE: &str = "SHARKSWAP_TEST_SERVICE";
#[cfg(any(test, feature = "test-hooks"))]
const HOOK_SECURITY: &str = "SHARKSWAP_TEST_SECURITY";
#[cfg(any(test, feature = "test-hooks"))]
const HOOK_DARWIN: &str = "SHARKSWAP_TEST_DARWIN";
#[cfg(any(test, feature = "test-hooks"))]
pub const HOOK_NOW: &str = "SHARKSWAP_NOW";
#[cfg(any(test, feature = "test-hooks"))]
const HOOK_LOCK_ATTEMPTS: &str = "SHARKSWAP_LOCK_ATTEMPTS";
#[cfg(any(test, feature = "test-hooks"))]
const HOOK_LOCK_DELAY: &str = "SHARKSWAP_LOCK_DELAY";
#[cfg(any(test, feature = "test-hooks"))]
pub const HOOK_SWITCH_FAULT: &str = "SHARKSWAP_TEST_SWITCH_FAULT";

/// The value of test hook `name` in the environment `var`: none unless the flag
/// is `1`, and none when the value is empty.
#[cfg(any(test, feature = "test-hooks"))]
fn hook_in(var: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    var(HOOKS).filter(|v| v == "1")?;
    var(name).filter(|v| !v.is_empty())
}

#[cfg(any(test, feature = "test-hooks"))]
pub fn hook(name: &str) -> Option<String> {
    hook_in(&|k| std::env::var(k).ok(), name)
}

/// Whether the login lives in the Keychain: on a Mac, or where a test forces it.
fn on_mac() -> bool {
    #[cfg(any(test, feature = "test-hooks"))]
    if hook(HOOK_DARWIN).is_some_and(|v| v == "1") {
        return true;
    }
    cfg!(target_os = "macos")
}

/// The `security` program to run.
fn security_program() -> String {
    #[cfg(any(test, feature = "test-hooks"))]
    if let Some(program) = hook(HOOK_SECURITY) {
        return program;
    }
    SECURITY.to_string()
}

/// Under `cargo test` the Keychain code may run only against a stub or a
/// throwaway keychain that the test hooks name. A test that got here any other
/// way would read or overwrite the developer's own login on a Mac, so it stops.
#[cfg(test)]
fn refuse_real_keychain(var: &dyn Fn(&str) -> Option<String>) {
    assert!(
        hook_in(var, HOOK_KEYCHAIN).is_some() || hook_in(var, HOOK_SECURITY).is_some(),
        "refusing to touch the real Keychain from a test: set {HOOKS}=1 and {HOOK_KEYCHAIN} \
         (a throwaway keychain) or {HOOK_SECURITY} (a stub) first, or set `darwin` to false"
    );
}

/// Which Keychain item to use: the Claude Code item in the default keychain,
/// unless a test hook names another.
pub struct KeychainTarget {
    service: String,
    keychain: Option<String>,
}

impl KeychainTarget {
    fn plain() -> KeychainTarget {
        KeychainTarget {
            service: KEYCHAIN_SVC.into(),
            keychain: None,
        }
    }

    #[cfg(not(any(test, feature = "test-hooks")))]
    pub fn from_env() -> KeychainTarget {
        KeychainTarget::plain()
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn from_env() -> KeychainTarget {
        KeychainTarget::from_lookup(&|k| std::env::var(k).ok())
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn from_lookup(var: &dyn Fn(&str) -> Option<String>) -> KeychainTarget {
        let plain = KeychainTarget::plain();
        KeychainTarget {
            service: hook_in(var, HOOK_SERVICE).unwrap_or(plain.service),
            keychain: hook_in(var, HOOK_KEYCHAIN),
        }
    }
}

fn keychain_read() -> Option<String> {
    #[cfg(test)]
    refuse_real_keychain(&|k| std::env::var(k).ok());
    let target = KeychainTarget::from_env();
    let mut cmd = Command::new(security_program());
    cmd.args(["find-generic-password", "-s", &target.service, "-w"]);
    cmd.args(&target.keychain);
    let out = cmd.stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(security_password(&out.stdout))
}

/// The password that `security find-generic-password -w` printed. It prints the
/// bytes as they are when every one is printable ASCII, and as lower-case hex
/// (`%02x` each) when any one is not (`do_password_item_printing` in
/// SecurityTool/macOS/keychain_find.c), so a login holding non-ASCII text comes
/// back as hex. The item is a JSON object, so printed as it is it starts with
/// `{`, which is not a hex digit. Hex is decoded only when the bytes it stands
/// for are UTF-8 text that starts with `{` and holds a byte that is not
/// printable ASCII (the reason `security` printed hex at all). Anything else is
/// returned as printed, and the caller then finds no valid login in it.
fn security_password(stdout: &[u8]) -> String {
    let text = String::from_utf8_lossy(stdout);
    let text = trim_nl(&text);
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    if text.len().is_multiple_of(2) {
        let bytes: Option<Vec<u8>> = text
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[hi, lo]| Some((digit(*hi)? << 4) | digit(*lo)?))
            .collect();
        if let Some(bytes) = bytes.filter(|b| b.iter().any(|c| !(0x20..=0x7e).contains(c)))
            && let Ok(decoded) = String::from_utf8(bytes)
            && decoded.starts_with('{')
        {
            return decoded;
        }
    }
    text.to_string()
}

/// The longest line `security -i` reads whole: 4094 bytes, not counting the
/// newline. Its `readline` keeps at most 4095 bytes of a line into a 4096-byte
/// buffer and leaves the rest on stdin, where the next `readline` would take it
/// for a second command line; so a longer line is never sent, only a line that
/// fits whole.
const SECURITY_LINE_MAX: usize = 4094;

/// One word of a `security -i` command line, double-quoted.
///
/// The tool's line parser is `split_line` in Apple's open-source `Security`
/// package (SecurityTool/macOS/security.c, github.com/apple-oss-distributions/
/// Security): inside a double-quoted word a backslash makes the next character
/// literal, the closing `"` ends the word, and every other byte is kept as it
/// is. So only a backslash (written `\\`) and a double quote (written `\"`)
/// need an escape; spaces, single quotes, braces and non-ASCII text pass
/// through. A line is what `readline` (readline.c in the same directory)
/// returns, up to the first `\n`, and the words are C strings, so a newline, a
/// carriage return or a NUL cannot be carried and is refused. Quoting every
/// word, even `-U`, keeps the parser out of its unquoted state, which calls
/// `isspace` on raw bytes.
pub fn security_quote(word: &str) -> Result<String, String> {
    if word.bytes().any(|b| matches!(b, b'\n' | b'\r' | 0)) {
        return Err("a value holds a line break or NUL, which `security -i` cannot take".into());
    }
    let mut out = String::with_capacity(word.len() + 2);
    out.push('"');
    for c in word.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Ok(out)
}

/// How to hand the credentials to `security`.
pub enum SecurityWrite {
    /// The whole `security -i` input, newline-terminated: the credentials ride
    /// on its stdin and are never in an argument list.
    Line(String),
    /// The arguments of `add-generic-password`, the credentials as the value of
    /// `-w` (what the bash version always did): for credentials whose line is
    /// over `SECURITY_LINE_MAX`.
    Argv(Vec<String>),
}

/// The command that stores `json` as the generic password of `account` in the
/// target item. The rule is on the whole line, not on the JSON: the line is
/// `add-generic-password -U -s SERVICE -a ACCOUNT -w JSON`, every word in
/// double quotes, with a backslash put before each `"` and `\`. When that line
/// is at most `SECURITY_LINE_MAX` bytes it is sent on stdin to `security -i`;
/// when it is longer, the arguments are returned for the argv form instead. A
/// multi-line JSON (only whitespace between tokens can break a line) is made
/// compact first; one that is not JSON, or that holds a NUL, is refused.
pub fn security_write(
    target: &KeychainTarget,
    account: &str,
    json: &str,
) -> Result<SecurityWrite, String> {
    let compact;
    let json = if json.contains(['\n', '\r']) {
        compact = serde_json::from_str::<Value>(json)
            .and_then(|v| serde_json::to_string(&v))
            .map_err(|_| "the credentials hold a line break and are not valid JSON".to_string())?;
        &compact
    } else {
        json
    };
    let mut words = vec![
        "add-generic-password",
        "-U",
        "-s",
        &target.service,
        "-a",
        account,
        "-w",
        json,
    ];
    words.extend(target.keychain.as_deref());
    let quoted: Result<Vec<String>, String> = words.iter().map(|w| security_quote(w)).collect();
    let line = quoted?.join(" ");
    if line.len() <= SECURITY_LINE_MAX {
        return Ok(SecurityWrite::Line(line + "\n"));
    }
    Ok(SecurityWrite::Argv(
        words.into_iter().map(String::from).collect(),
    ))
}

/// The most of `security`'s stderr put in an error message.
const STDERR_MAX: usize = 300;

/// `security`'s stderr as one line of printable text, cut to `STDERR_MAX` bytes.
fn stderr_excerpt(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let one_line = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if one_line.len() <= STDERR_MAX {
        return one_line;
    }
    let mut end = STDERR_MAX;
    while !one_line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &one_line[..end])
}

/// Start `program` with `args`; its stdout is discarded and its stderr is kept.
/// `piped` gives it a stdin pipe, else stdin is empty.
fn security_spawn(program: &str, args: &[String], piped: bool) -> Result<Child, String> {
    Command::new(program)
        .args(args)
        .stdin(if piped { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {program}: {}", os_msg(&e)))
}

/// Feed `input` (when given) to the child's stdin, wait for it, and say what
/// went wrong, with its stderr, if anything did. The text is never the input.
/// SIGPIPE is ignored for the write (this program leaves it at its default), so
/// a child that died early is a failure to report and not the end of this
/// process. For `security -i` the exit status is that of the last command it ran.
fn security_finish(mut child: Child, input: Option<&str>) -> Result<(), String> {
    let mut sent = true;
    if let Some(input) = input {
        // SAFETY: signal() only swaps a disposition; the old one is put back below.
        let old = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
        sent = child
            .stdin
            .take()
            .is_some_and(|mut pipe| pipe.write_all(input.as_bytes()).is_ok());
        // SAFETY: `old` is the disposition returned above.
        unsafe { libc::signal(libc::SIGPIPE, old) };
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("cannot wait for security: {}", os_msg(&e)))?;
    let said = stderr_excerpt(&out.stderr);
    let tail = if said.is_empty() {
        String::new()
    } else {
        format!(": {said}")
    };
    match out.status.code() {
        Some(0) if sent => Ok(()),
        Some(0) => Err(format!("the command could not be sent to security{tail}")),
        Some(code) => Err(format!("security exited with status {code}{tail}")),
        None => Err(format!("security was ended by a signal{tail}")),
    }
}

/// Run `program` as `how` says. The argv form prints its one warning here, once
/// the child exists: from then on the credentials were on a command line.
fn security_store(program: &str, how: &SecurityWrite) -> Result<(), String> {
    match how {
        SecurityWrite::Line(line) => {
            let child = security_spawn(program, &["-i".to_string()], true)?;
            security_finish(child, Some(line))
        }
        SecurityWrite::Argv(args) => {
            let child = security_spawn(program, args, false)?;
            eprintln!(
                "warning: the credentials are too large for one `security -i` line (the limit is \
                 {SECURITY_LINE_MAX} bytes), so they were passed to `security` on its command \
                 line and were briefly visible to other local users in the process list"
            );
            security_finish(child, None)
        }
    }
}

/// Store `json` as the Claude Code Keychain item. Prints its own errors.
fn keychain_write(json: &str) -> Result<(), i32> {
    #[cfg(test)]
    refuse_real_keychain(&|k| std::env::var(k).ok());
    let user = std::env::var("USER").unwrap_or_default();
    let target = KeychainTarget::from_env();
    let how = match security_write(&target, &user, json) {
        Ok(h) => h,
        Err(why) => {
            eprintln!("error: Keychain write refused: {why}");
            return Err(1);
        }
    };
    if let Err(why) = security_store(&security_program(), &how) {
        eprintln!(
            "error: Keychain write failed (locked? run from a GUI session on this Mac): {why}"
        );
        return Err(1);
    }
    Ok(())
}

/// The prior Keychain text, for a rollback; None when it cannot be read.
pub fn keychain_prior() -> Option<String> {
    keychain_read()
}

/// `.claudeAiOauth` holds a usable login: strings for both tokens, a whole
/// non-negative expiry.
pub fn valid_oauth(v: &Value) -> bool {
    let Some(Value::Object(o)) = v.get("claudeAiOauth") else {
        return false;
    };
    let s = |k: &str| {
        o.get(k)
            .and_then(Value::as_str)
            .is_some_and(|t| !t.is_empty())
    };
    s("accessToken")
        && s("refreshToken")
        && o.get("expiresAt")
            .and_then(Value::as_f64)
            .is_some_and(safe_whole)
}

/// `valid_oauth` over JSON text (the last value counts, as `jq -e` judges it).
pub fn valid_oauth_text(text: &str) -> bool {
    last_json(text).is_some_and(|v| valid_oauth(&v))
}

pub fn valid_oauth_file(path: &Path) -> bool {
    read_last_json(path).is_some_and(|v| valid_oauth(&v))
}

/// An object with a non-empty string `emailAddress`.
pub fn valid_metadata(v: &Value) -> bool {
    v.get("emailAddress")
        .and_then(Value::as_str)
        .is_some_and(|e| !e.is_empty())
        && v.is_object()
}

pub fn valid_metadata_file(path: &Path) -> bool {
    read_last_json(path).is_some_and(|v| valid_metadata(&v))
}

/// `restore_path`: put the backup back, or remove what was never there.
pub fn restore_path(dst: &Path, backup: &Path, had: bool) -> bool {
    if had {
        copy_p(backup, dst).is_ok()
    } else {
        std::fs::remove_file(dst).is_ok() || !dst.exists()
    }
}

// ---- aliases ----------------------------------------------------------------

/// Every word the dispatch answers as a command (keep in step with main.rs).
pub const COMMAND_WORDS: &[&str] = &[
    "next",
    "toggle",
    "status",
    "st",
    "list",
    "ls",
    "pane",
    "save",
    "watch",
    "best",
    "bar",
    "token",
    "probe",
    "rotate",
    "remove",
    "rm",
    "help",
    "-h",
    "--help",
    "--version",
    "-V",
];

/// `check_alias` without the message: safe characters, no reserved name, and
/// (unless removing) no command word.
pub fn alias_ok(alias: &str, remove: bool) -> bool {
    let safe = !alias.is_empty()
        && alias
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && !alias.starts_with('.')
        && alias != "availability"
        && alias != "account-caps"
        && !alias.ends_with(".creds");
    safe && (remove || !COMMAND_WORDS.contains(&alias))
}

/// `check_alias`: `alias_ok`, and the error message when it fails.
pub fn check_alias(alias: &str, remove: bool) -> Result<(), i32> {
    if !alias_ok(alias, remove) {
        eprintln!("error: invalid or reserved alias '{alias}'");
        return Err(1);
    }
    Ok(())
}

pub enum Spelling {
    Exact,
    Absent,
    Variant(String),
}

/// `alias_spelling`: exact match in any directory, else the stored spelling that
/// differs only by case, else absent.
pub fn alias_spelling(alias: &str, dirs: &[&Path]) -> Spelling {
    let mut variant: Option<String> = None;
    let lower = alias.to_ascii_lowercase();
    for dir in dirs {
        for f in glob_json(dir) {
            let b = &f[..f.len() - 5];
            if b == alias {
                return Spelling::Exact;
            }
            if b.to_ascii_lowercase() == lower {
                variant = Some(b.to_string());
            }
        }
    }
    variant.map_or(Spelling::Absent, Spelling::Variant)
}

/// `alias_exact`: refuse an alias spelled only up to case. Prints its error.
pub fn alias_exact(alias: &str, dirs: &[&Path]) -> Result<(), i32> {
    if let Spelling::Variant(spelled) = alias_spelling(alias, dirs) {
        eprintln!(
            "error: unknown account '{alias}': the store has '{spelled}', and aliases are case-sensitive"
        );
        return Err(1);
    }
    Ok(())
}

// ---- the identity lock ----------------------------------------------------------

/// The lock's owner file, its directory, and the switch-intent marker inside it.
static LOCK_PATHS: OnceLock<(std::ffi::CString, std::ffi::CString, std::ffi::CString)> =
    OnceLock::new();
static LOCK_HELD: AtomicBool = AtomicBool::new(false);

/// Whether this process holds the identity lock right now.
pub fn lock_held_here() -> bool {
    LOCK_HELD.load(Ordering::SeqCst)
}

/// Whether `path` exists as itself (a link is not followed). `lstat` is
/// async-signal-safe, so the signal handler may ask.
fn exists_lstat(path: &std::ffi::CStr) -> bool {
    // SAFETY: lstat takes a valid C string and a writable stat buffer.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::lstat(path.as_ptr(), &mut st) == 0
    }
}

extern "C" fn on_signal(_sig: libc::c_int) {
    // A lock that holds a switch-intent marker stays, owner file and all: the
    // switch was cut short, and the next run must find that out.
    if LOCK_HELD.load(Ordering::SeqCst)
        && let Some((owner, dir, pending)) = LOCK_PATHS.get()
        && !exists_lstat(pending)
    {
        // SAFETY: unlink and rmdir are async-signal-safe and take valid C strings.
        unsafe {
            libc::unlink(owner.as_ptr());
            libc::rmdir(dir.as_ptr());
        }
    }
    // SAFETY: _exit is async-signal-safe.
    unsafe { libc::_exit(130) };
}

/// How long the identity lock is waited for: 50 tries, 0.1 s apart. A test hook
/// can change both; a build without the hooks always gets the defaults.
fn lock_patience() -> (i64, f64) {
    #[cfg(any(test, feature = "test-hooks"))]
    {
        let tries = hook(HOOK_LOCK_ATTEMPTS)
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(50);
        let delay = hook(HOOK_LOCK_DELAY)
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|d| d.is_finite() && *d >= 0.0)
            .unwrap_or(0.1);
        (tries, delay)
    }
    #[cfg(not(any(test, feature = "test-hooks")))]
    {
        (50, 0.1)
    }
}

/// A held identity lock: a directory with an `owner` file naming this process.
/// Dropping it releases it, as the shell's EXIT trap did.
pub struct IdLock {
    dir: PathBuf,
    owner: String,
    held: bool,
    signals: SavedSignals,
}

impl IdLock {
    pub fn acquire(ctx: &Ctx) -> Result<IdLock, i32> {
        Self::acquire_within(ctx, lock_patience())
    }

    /// [`acquire`](Self::acquire) with the patience given as `(tries, seconds
    /// between them)`, so a test need not race the default.
    fn acquire_within(ctx: &Ctx, (tries, delay): (i64, f64)) -> Result<IdLock, i32> {
        let dir = crate::switch_intent::lock_dir(&ctx.store);
        let owner_file = dir.join("owner");
        let (mut i, mut hard) = (0i64, 0u32);
        loop {
            // A switch that was cut short holds the lock until a person has
            // looked. One that is still running is waited for like any holder.
            crate::switch_intent::ready_to_wait(&ctx.store)?;
            let err: String;
            match std::fs::create_dir(&dir) {
                Ok(()) => match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&owner_file)
                {
                    Ok(_) => break,
                    Err(e) => {
                        err = format!("{}: {}", owner_file.display(), os_msg(&e));
                        if owner_file.exists() {
                            hard = 0
                        } else {
                            hard += 1
                        }
                        let _ = std::fs::remove_dir(&dir);
                    }
                },
                Err(e) => {
                    err = format!(
                        "mkdir: cannot create directory '{}': {}",
                        dir.display(),
                        os_msg(&e)
                    );
                    if dir.exists() { hard = 0 } else { hard += 1 }
                }
            }
            if hard >= 3 {
                eprintln!(
                    "error: cannot take the identity lock at {}: {err}",
                    dir.display()
                );
                return Err(1);
            }
            if hard > 0 {
                continue;
            }
            i += 1;
            if i >= tries {
                let owner = read_text(&owner_file)
                    .map(|t| shown_line(&t))
                    .unwrap_or_default();
                let tail = if owner.is_empty() {
                    String::new()
                } else {
                    format!(" (owner: {owner})")
                };
                eprintln!("error: identity lock timed out{tail}; refusing to steal it");
                return Err(1);
            }
            std::thread::sleep(std::time::Duration::from_secs_f64(delay));
        }
        let owner = format!(
            "pid={} started={}",
            std::process::id(),
            iso_secs(wall_secs())
        );
        if std::fs::write(&owner_file, format!("{owner}\n")).is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(1);
        }
        if let (Ok(o), Ok(d), Ok(p)) = (
            std::ffi::CString::new(owner_file.as_os_str().as_encoded_bytes()),
            std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()),
            std::ffi::CString::new(
                crate::switch_intent::marker(&ctx.store)
                    .as_os_str()
                    .as_encoded_bytes(),
            ),
        ) {
            let _ = LOCK_PATHS.set((o, d, p));
        }
        LOCK_HELD.store(true, Ordering::SeqCst);
        Ok(IdLock {
            dir,
            owner,
            held: true,
            signals: catch_signals(on_signal),
        })
    }

    /// Release now (the shell's `identity_lock_done`).
    pub fn done(mut self) {
        self.release();
    }

    fn release(&mut self) {
        if !self.held {
            return;
        }
        self.held = false;
        LOCK_HELD.store(false, Ordering::SeqCst);
        self.signals.restore();
        let owner_file = self.dir.join("owner");
        if read_text(&owner_file).is_ok_and(|t| trim_nl(&t) == self.owner) {
            if crate::switch_intent::pending_at(&self.dir) {
                eprintln!(
                    "error: unfinished account switch; identity lock retained at {} for inspection",
                    self.dir.display()
                );
            } else {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }
    }
}

impl Drop for IdLock {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{
        TestStore, disposition, end, lock_of_running_switch, signal_lock, sleeper, spawn_lock,
    };
    use serde_json::json;
    use std::os::unix::fs::MetadataExt;
    use std::sync::{Mutex, MutexGuard, PoisonError};

    fn target() -> KeychainTarget {
        KeychainTarget {
            service: KEYCHAIN_SVC.into(),
            keychain: None,
        }
    }

    /// A port of `split_line` from Apple's SecurityTool/macOS/security.c, the
    /// parser `security -i` runs on each input line, so the tests can check
    /// that a quoted line comes back as the exact arguments that went in.
    fn apple_split_line(line: &[u8]) -> Vec<Vec<u8>> {
        #[derive(PartialEq)]
        enum State {
            SkipWs,
            ReadArg,
            ReadArgEscaped,
            QuotedArg,
            QuotedArgEscaped,
        }
        let is_space = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r');
        let (mut args, mut cur): (Vec<Vec<u8>>, Vec<u8>) = (Vec::new(), Vec::new());
        let (mut state, mut quote) = (State::SkipWs, 0u8);
        for &c in line {
            if state == State::SkipWs {
                if is_space(c) {
                    continue;
                }
                if c == b'"' || c == b'\'' {
                    quote = c;
                    state = State::QuotedArg;
                    continue;
                }
                state = State::ReadArg;
            }
            if state == State::ReadArg {
                if c == b'\\' {
                    state = State::ReadArgEscaped;
                    continue;
                } else if is_space(c) {
                    args.push(std::mem::take(&mut cur));
                    state = State::SkipWs;
                } else {
                    cur.push(c);
                }
            }
            if state == State::QuotedArg {
                if c == b'\\' {
                    state = State::QuotedArgEscaped;
                    continue;
                }
                if c == quote {
                    args.push(std::mem::take(&mut cur));
                    state = State::SkipWs;
                } else {
                    cur.push(c);
                }
            }
            if state == State::ReadArgEscaped {
                cur.push(c);
                state = State::ReadArg;
            }
            if state == State::QuotedArgEscaped {
                cur.push(c);
                state = State::QuotedArg;
            }
        }
        if state != State::SkipWs {
            args.push(cur);
        }
        args
    }

    fn words(line: &str) -> Vec<String> {
        apple_split_line(line.trim_end_matches('\n').as_bytes())
            .into_iter()
            .map(|w| String::from_utf8(w).expect("utf-8"))
            .collect()
    }

    #[test]
    fn security_quote_escapes_only_quote_and_backslash() {
        let cases = [
            ("plain", r#""plain""#),
            ("", r#""""#),
            ("two words", r#""two words""#),
            ("it's", r#""it's""#),
            (r#"say "hi""#, r#""say \"hi\"""#),
            (r"back\slash", r#""back\\slash""#),
            (r"ends with \", r#""ends with \\""#),
            (r#"\""#, r#""\\\"""#),
            ("{\"a\": [1, 2]}", r#""{\"a\": [1, 2]}""#),
            ("caf\u{e9} \u{1f511}", "\"caf\u{e9} \u{1f511}\""),
            ("tab\there", "\"tab\there\""),
        ];
        for (raw, want) in cases {
            assert_eq!(security_quote(raw).as_deref(), Ok(want), "quoting {raw:?}");
        }
    }

    #[test]
    fn security_quote_refuses_what_a_line_cannot_carry() {
        for bad in ["a\nb", "a\rb", "a\0b", "\n"] {
            assert!(security_quote(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn a_quoted_word_is_read_back_exactly_by_the_security_parser() {
        let mut corpus: Vec<String> = [
            "plain",
            "",
            "two  words",
            "it's \"quoted\" and \\ slashed",
            "\\",
            "\\\\",
            "\"",
            "'",
            "end\\",
            "# not a comment ; $HOME `x` | & > <",
            "caf\u{e9} \u{1f511}",
            "tab\tand\x0bvt",
        ]
        .map(String::from)
        .to_vec();
        corpus.push((0x20u8..0x7f).map(char::from).collect());
        corpus.push(
            json!({"claudeAiOauth": {"accessToken": "sk-ant-oat01-a\"b\\c 'd'", "scopes": ["x y"]}})
                .to_string(),
        );
        for raw in &corpus {
            let q = security_quote(raw).expect("quotable");
            assert_eq!(words(&q), vec![raw.clone()], "round trip of {raw:?}");
        }
    }

    /// The `security -i` line for `json`; the credentials must fit on one.
    fn line_of(t: &KeychainTarget, account: &str, json: &str) -> String {
        match security_write(t, account, json).expect("a command") {
            SecurityWrite::Line(line) => line,
            SecurityWrite::Argv(_) => panic!("expected the stdin form"),
        }
    }

    /// The `add-generic-password` arguments for `json` in the argv form.
    fn argv_of(t: &KeychainTarget, account: &str, json: &str) -> Vec<String> {
        match security_write(t, account, json).expect("a command") {
            SecurityWrite::Argv(args) => args,
            SecurityWrite::Line(_) => panic!("expected the argv form"),
        }
    }

    #[test]
    fn the_add_line_parses_back_to_the_command_and_its_arguments() {
        let json = json!({
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-with \"quote\" and \\ and 'single'",
                "refreshToken": "sk-ant-ort01-x y",
                "expiresAt": 1_900_000_000_000_u64,
            },
            "note": "caf\u{e9}",
        })
        .to_string();
        let line = line_of(&target(), "some user", &json);
        assert!(line.ends_with('\n') && line.matches('\n').count() == 1);
        assert_eq!(
            words(&line),
            [
                "add-generic-password",
                "-U",
                "-s",
                KEYCHAIN_SVC,
                "-a",
                "some user",
                "-w",
                &json
            ]
        );
    }

    #[test]
    fn the_add_line_names_a_keychain_last_when_the_test_hook_gives_one() {
        let t = KeychainTarget {
            service: "svc".into(),
            keychain: Some("/tmp/kc dir/t.keychain".into()),
        };
        let line = line_of(&t, "u", "{}");
        let w = words(&line);
        assert_eq!(w[3], "svc");
        assert_eq!(w.last().map(String::as_str), Some("/tmp/kc dir/t.keychain"));
        assert_eq!(w.len(), 9);
    }

    #[test]
    fn a_multi_line_json_is_made_compact_and_a_non_json_one_is_refused() {
        let pretty = "{\n  \"claudeAiOauth\": {\n    \"accessToken\": \"a b\"\n  }\n}\n";
        let line = line_of(&target(), "u", pretty);
        let w = words(&line);
        assert!(!w[7].contains('\n'));
        assert_eq!(
            serde_json::from_str::<Value>(&w[7]).expect("json"),
            serde_json::from_str::<Value>(pretty).expect("json")
        );
        assert!(security_write(&target(), "u", "not json\nat all").is_err());
    }

    /// The rule is on the whole quoted line, not on the JSON: a line of exactly
    /// 4094 bytes (the newline not counted) goes to stdin, one byte more takes
    /// the argv form, and neither is ever cut.
    #[test]
    fn the_size_decision_is_made_on_the_whole_line_at_the_limit() {
        let overhead = line_of(&target(), "u", "").len() - 1;
        let fits = "x".repeat(SECURITY_LINE_MAX - overhead);
        let line = line_of(&target(), "u", &fits);
        assert_eq!(line.len() - 1, SECURITY_LINE_MAX);
        assert_eq!(words(&line)[7], fits);
        let over = "x".repeat(SECURITY_LINE_MAX - overhead + 1);
        let args = argv_of(&target(), "u", &over);
        assert_eq!(
            args,
            [
                "add-generic-password",
                "-U",
                "-s",
                KEYCHAIN_SVC,
                "-a",
                "u",
                "-w",
                &over
            ]
        );
    }

    /// The backslash put before every `"` and `\` is what makes a JSON shorter than
    /// the limit still not fit: the decision follows the quoted size.
    #[test]
    fn escaping_counts_toward_the_limit() {
        let overhead = line_of(&target(), "u", "").len() - 1;
        let room = SECURITY_LINE_MAX - overhead;
        let quotes = "\"".repeat(room / 2 + 1);
        assert!(quotes.len() <= room, "shorter than the room left");
        argv_of(&target(), "u", &quotes);
        let fits = "\"".repeat(room / 2);
        assert_eq!(
            line_of(&target(), "u", &fits).len() - 1,
            overhead + 2 * (room / 2)
        );
    }

    #[test]
    fn the_argv_form_for_a_keychain_names_it_last() {
        let t = KeychainTarget {
            service: "svc".into(),
            keychain: Some("/tmp/t.keychain".into()),
        };
        let args = argv_of(&t, "u", &"x".repeat(SECURITY_LINE_MAX));
        assert_eq!(args.last().map(String::as_str), Some("/tmp/t.keychain"));
        assert_eq!(args[7].len(), SECURITY_LINE_MAX);
    }

    fn stub_security(dir: &Path, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("security");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn the_secret_goes_to_security_on_stdin_and_never_on_its_argv() {
        let _spawn = spawn_lock();
        let dir = TestStore::new(&[]);
        let argv_file = dir.join("argv");
        let stdin_file = dir.join("stdin");
        let program = stub_security(
            &dir,
            &format!(
                "printf '%s\\n' \"$@\" > '{}'\ncat > '{}'",
                argv_file.display(),
                stdin_file.display()
            ),
        );
        let json = r#"{"accessToken":"sk-ant-secret"}"#;
        let how = security_write(&target(), "u", json).expect("a command");
        security_store(&program, &how).expect("stored");
        let SecurityWrite::Line(line) = how else {
            panic!("expected the stdin form");
        };
        assert_eq!(std::fs::read_to_string(&argv_file).expect("argv"), "-i\n");
        assert_eq!(std::fs::read_to_string(&stdin_file).expect("stdin"), line);
    }

    #[test]
    fn a_too_long_line_takes_the_argv_form_with_nothing_on_stdin() {
        let _spawn = spawn_lock();
        let dir = TestStore::new(&[]);
        let argv_file = dir.join("argv");
        let stdin_file = dir.join("stdin");
        let program = stub_security(
            &dir,
            &format!(
                "printf '%s\\n' \"$@\" > '{}'\ncat > '{}'",
                argv_file.display(),
                stdin_file.display()
            ),
        );
        let json = format!(r#"{{"k":"{}"}}"#, "x".repeat(SECURITY_LINE_MAX));
        let how = security_write(&target(), "u", &json).expect("a command");
        security_store(&program, &how).expect("stored");
        let argv = std::fs::read_to_string(&argv_file).expect("argv");
        let want = format!("add-generic-password\n-U\n-s\n{KEYCHAIN_SVC}\n-a\nu\n-w\n{json}\n");
        assert_eq!(argv, want);
        assert_eq!(std::fs::read_to_string(&stdin_file).expect("stdin"), "");
    }

    #[test]
    fn a_failing_security_is_a_failure_with_its_stderr_and_a_missing_one_too() {
        let _spawn = spawn_lock();
        let dir = TestStore::new(&[]);
        let how = SecurityWrite::Line("line\n".into());
        let failing = stub_security(&dir, "cat > /dev/null\necho 'item is locked' >&2\nexit 36");
        let why = security_store(&failing, &how).expect_err("fails");
        assert_eq!(why, "security exited with status 36: item is locked");
        let silent = stub_security(&dir, "cat > /dev/null\nexit 1");
        assert_eq!(
            security_store(&silent, &how).expect_err("fails"),
            "security exited with status 1"
        );
        let why = security_store(&dir.join("does-not-exist").to_string_lossy(), &how)
            .expect_err("cannot run");
        assert!(why.starts_with("cannot run "), "{why}");
    }

    #[test]
    fn the_stderr_excerpt_is_one_short_printable_line_and_never_the_input() {
        let _spawn = spawn_lock();
        let dir = TestStore::new(&[]);
        let long = "e".repeat(2000);
        let program = stub_security(
            &dir,
            &format!("cat > /dev/null\nprintf 'bad\\n\\tthing\\n%s\\n' {long} >&2\nexit 1"),
        );
        let how = SecurityWrite::Line("add-generic-password -w SECRET-TOKEN\n".into());
        let why = security_store(&program, &how).expect_err("fails");
        assert!(
            why.starts_with("security exited with status 1: bad thing eee"),
            "{why}"
        );
        assert!(why.ends_with("..."), "{why}");
        assert!(why.len() < 400, "capped: {} bytes", why.len());
        assert!(!why.contains("SECRET-TOKEN"));
        assert!(!why.contains('\n'));
    }

    #[test]
    fn the_stderr_excerpt_cuts_on_a_character_boundary() {
        let text = "\u{e9}".repeat(STDERR_MAX);
        let cut = stderr_excerpt(text.as_bytes());
        assert!(cut.ends_with("..."));
        assert!(cut.len() <= STDERR_MAX + 3);
        assert_eq!(stderr_excerpt(b"short\n"), "short");
    }

    fn hook_env(flag: Option<&'static str>) -> impl Fn(&str) -> Option<String> {
        move |k: &str| match k {
            "SHARKSWAP_TEST_HOOKS" => flag.map(String::from),
            "SHARKSWAP_TEST_KEYCHAIN" => Some("/tmp/t.keychain".into()),
            "SHARKSWAP_TEST_SERVICE" => Some("test-service".into()),
            "SHARKSWAP_TEST_SECURITY" => Some("/tmp/stub-security".into()),
            "SHARKSWAP_TEST_DARWIN" => Some("1".into()),
            "SHARKSWAP_NOW" => Some("1800000000".into()),
            "SHARKSWAP_LOCK_ATTEMPTS" => Some("2".into()),
            "SHARKSWAP_LOCK_DELAY" => Some("0.01".into()),
            "SHARKSWAP_TEST_SWITCH_FAULT" => Some("creds:fail".into()),
            _ => None,
        }
    }

    #[test]
    fn the_test_hooks_are_honoured_only_with_their_flag() {
        for flag in [None, Some("0"), Some("")] {
            let env = hook_env(flag);
            let t = KeychainTarget::from_lookup(&env);
            assert_eq!(t.service, KEYCHAIN_SVC, "flag {flag:?}");
            assert_eq!(t.keychain, None, "flag {flag:?}");
            for name in [
                HOOK_KEYCHAIN,
                HOOK_SERVICE,
                HOOK_SECURITY,
                HOOK_DARWIN,
                HOOK_NOW,
                HOOK_LOCK_ATTEMPTS,
                HOOK_LOCK_DELAY,
                HOOK_SWITCH_FAULT,
            ] {
                assert_eq!(hook_in(&env, name), None, "{name} with flag {flag:?}");
            }
        }
        let env = hook_env(Some("1"));
        let t = KeychainTarget::from_lookup(&env);
        assert_eq!(t.service, "test-service");
        assert_eq!(t.keychain.as_deref(), Some("/tmp/t.keychain"));
        assert_eq!(
            hook_in(&env, HOOK_SECURITY).as_deref(),
            Some("/tmp/stub-security")
        );
        assert_eq!(hook_in(&env, HOOK_DARWIN).as_deref(), Some("1"));
        assert_eq!(hook_in(&env, HOOK_NOW).as_deref(), Some("1800000000"));
        assert_eq!(hook_in(&env, HOOK_LOCK_ATTEMPTS).as_deref(), Some("2"));
        assert_eq!(hook_in(&env, HOOK_LOCK_DELAY).as_deref(), Some("0.01"));
    }

    #[test]
    fn the_real_security_is_called_by_absolute_path() {
        assert!(SECURITY.starts_with('/'));
        // No test hook is set in the unit tests' own environment, so the
        // program is the system one, never a name that PATH could redirect.
        if std::env::var_os(HOOKS).is_none() {
            assert_eq!(security_program(), SECURITY);
        }
    }

    /// Under `cargo test` the Keychain code is reachable only through a hook
    /// that names a stub or a throwaway keychain.
    #[test]
    fn a_test_cannot_reach_the_real_keychain() {
        let refused = std::panic::catch_unwind(|| refuse_real_keychain(&|_| None));
        assert!(refused.is_err(), "no hooks: refused");
        let flag_off = std::panic::catch_unwind(|| {
            refuse_real_keychain(&|k| match k {
                "SHARKSWAP_TEST_SECURITY" => Some("/tmp/stub".into()),
                _ => None,
            })
        });
        assert!(flag_off.is_err(), "a hook without the flag: refused");
        let only_service = std::panic::catch_unwind(|| {
            refuse_real_keychain(&|k| match k {
                "SHARKSWAP_TEST_HOOKS" => Some("1".into()),
                "SHARKSWAP_TEST_SERVICE" => Some("other".into()),
                _ => None,
            })
        });
        assert!(only_service.is_err(), "another service name alone: refused");
        refuse_real_keychain(&hook_env(Some("1")));
    }

    #[test]
    fn a_loose_store_directory_is_tightened_to_0700() {
        let home = TestStore::new(&[]);
        let store = home.join(".sharkswap");
        std::fs::create_dir(&store).expect("mkdir");
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let ctx = Ctx::at(home.to_path_buf(), true).unwrap_or_else(|_| panic!("a context"));
        assert_eq!(
            std::fs::metadata(&ctx.store).expect("stat").mode() & 0o777,
            0o700
        );
    }

    /// Where each of the signals the lock cleans up after stands, as a list.
    fn dispositions() -> Vec<libc::sighandler_t> {
        [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM]
            .map(disposition)
            .to_vec()
    }

    #[test]
    fn the_identity_lock_takes_the_signals_and_gives_them_back_on_release() {
        let _serial = signal_lock();
        let home = TestStore::new(&[]);
        let ctx = Ctx::at(home.to_path_buf(), true).unwrap_or_else(|_| panic!("a context"));
        let before = dispositions();
        let ours = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;

        let lock = IdLock::acquire(&ctx).unwrap_or_else(|_| panic!("the lock"));
        assert!(ctx.store.join(".identity.lock/owner").is_file());
        for (now, was) in dispositions().iter().zip(&before) {
            // A signal that was ignored on entry (nohup) stays ignored.
            assert!(*now == ours || (*was == libc::SIG_IGN && *now == libc::SIG_IGN));
        }
        lock.done();
        assert!(!ctx.store.join(".identity.lock").exists());
        assert_eq!(dispositions(), before, "the old dispositions are back");

        let lock = IdLock::acquire(&ctx).unwrap_or_else(|_| panic!("the lock again"));
        drop(lock);
        assert!(!ctx.store.join(".identity.lock").exists());
        assert_eq!(dispositions(), before, "a drop restores them too");
    }

    #[test]
    fn an_unfinished_switch_hides_the_live_identity_whatever_the_marker_is() {
        use crate::switch_intent::{lock_dir, marker};
        let home = TestStore::new(&[
            (
                ".claude.json",
                json!({"oauthAccount": {"emailAddress": "alice@example.com"}}),
            ),
            (
                ".sharkswap/work.json",
                json!({"emailAddress": "alice@example.com"}),
            ),
        ]);
        let ctx = Ctx::at(home.to_path_buf(), true).unwrap_or_else(|_| panic!("a context"));
        assert_eq!(ctx.current_email(), "alice@example.com");
        assert_eq!(ctx.current_alias(), "work");

        std::fs::create_dir(lock_dir(&ctx.store)).expect("a lock directory");
        let path = marker(&ctx.store);
        // A regular file is never opened, a directory and a dangling link count too.
        for what in ["a file", "a directory", "a dangling link"] {
            match what {
                "a file" => std::fs::write(&path, "").expect("file"),
                "a directory" => std::fs::create_dir(&path).expect("dir"),
                _ => std::os::unix::fs::symlink("/nonexistent/cs-intent", &path).expect("link"),
            }
            assert_eq!(ctx.current_email(), "", "{what}");
            assert_eq!(ctx.current_alias(), "", "{what}");
            if path.is_dir() {
                std::fs::remove_dir(&path).expect("rmdir");
            } else {
                std::fs::remove_file(&path).expect("rm");
            }
        }
        assert_eq!(
            ctx.current_email(),
            "alice@example.com",
            "after the marker is gone"
        );
        assert_eq!(ctx.current_alias(), "work");
    }

    #[test]
    fn an_unfinished_switch_keeps_the_identity_lock_and_refuses_the_next_one() {
        use crate::switch_intent::{clear, lock_dir, publish};
        let _serial = signal_lock();
        let home = TestStore::new(&[]);
        let ctx = Ctx::at(home.to_path_buf(), true).unwrap_or_else(|_| panic!("a context"));
        let dir = lock_dir(&ctx.store);

        let lock = IdLock::acquire(&ctx).unwrap_or_else(|_| panic!("the lock"));
        publish(&ctx.store, "work").expect("the intent");
        lock.done();
        assert!(
            dir.join("owner").is_file(),
            "the owner file stays with the marker"
        );
        assert!(dir.join("switch-pending").is_file(), "so does the marker");
        assert!(
            IdLock::acquire(&ctx).is_err(),
            "no one takes the lock, and no one waits for it"
        );

        // Dropped, as an early return would: the same.
        std::fs::remove_dir_all(&dir).expect("cleared by hand");
        let lock = IdLock::acquire(&ctx).unwrap_or_else(|_| panic!("the lock again"));
        publish(&ctx.store, "work").expect("the intent");
        drop(lock);
        assert!(dir.join("switch-pending").is_file());

        // A switch that finished clears the marker first, and then the lock goes.
        std::fs::remove_dir_all(&dir).expect("cleared by hand");
        let lock = IdLock::acquire(&ctx).unwrap_or_else(|_| panic!("the lock a third time"));
        publish(&ctx.store, "work").expect("the intent");
        clear(&ctx.store).expect("clear");
        lock.done();
        assert!(!dir.exists());
        IdLock::acquire(&ctx)
            .unwrap_or_else(|_| panic!("free again"))
            .done();
    }

    #[test]
    fn a_waiter_meets_a_running_switch_and_takes_the_lock_when_it_ends() {
        let _serial = signal_lock();
        let home = TestStore::new(&[]);
        let ctx = Ctx::at(home.to_path_buf(), true).unwrap_or_else(|_| panic!("a context"));
        let holder = sleeper();
        lock_of_running_switch(&ctx.store, holder.id());
        let dir = crate::switch_intent::lock_dir(&ctx.store);
        let marker = crate::switch_intent::marker(&ctx.store);
        let finisher = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            std::fs::remove_file(marker).expect("the marker goes first");
            std::fs::remove_dir_all(dir).expect("then the lock");
        });
        let started = std::time::Instant::now();
        // Patience for 30 s, though the holder lets go after 0.4 s: a long stall
        // under load must not turn the wait into a timeout.
        let lock = IdLock::acquire_within(&ctx, (300, 0.1))
            .unwrap_or_else(|_| panic!("the waiter should wait for a live switch, not refuse"));
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(300),
            "it really waited for the holder"
        );
        lock.done();
        finisher.join().expect("the finisher");
        end(holder);
    }

    #[test]
    fn a_symbolic_link_as_the_store_is_not_followed_to_change_its_mode() {
        let home = TestStore::new(&[]);
        let target_dir = home.join("elsewhere");
        std::fs::create_dir(&target_dir).expect("mkdir");
        std::fs::set_permissions(&target_dir, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        std::os::unix::fs::symlink(&target_dir, home.join(".sharkswap")).expect("symlink");
        let ctx = Ctx::at(home.to_path_buf(), true).unwrap_or_else(|_| panic!("a context"));
        assert!(ctx.store.is_dir());
        assert_eq!(
            std::fs::metadata(&target_dir).expect("stat").mode() & 0o777,
            0o755,
            "the directory the link points to keeps its mode"
        );
    }

    #[test]
    fn a_new_store_directory_is_private() {
        let home = TestStore::new(&[]);
        let ctx = Ctx::at(home.to_path_buf(), true).unwrap_or_else(|_| panic!("a context"));
        assert_eq!(
            std::fs::metadata(&ctx.store).expect("stat").mode() & 0o077,
            0
        );
    }

    #[test]
    fn creds_write_replaces_the_file_atomically_and_keeps_it_private() {
        let home = TestStore::new(&[]);
        let mut ctx = Ctx::at(home.to_path_buf(), true).unwrap_or_else(|_| panic!("a context"));
        // On a Mac `Ctx::at` sets this, and a write with no file goes to the
        // Keychain: this test is about the file, so it must not.
        ctx.darwin = false;
        assert!(ctx.creds_write(r#"{"a":1}"#).is_ok());
        assert!(ctx.creds_write(r#"{"a":2}"#).is_ok());
        assert_eq!(ctx.creds_read(), r#"{"a":2}"#);
        assert_eq!(
            std::fs::metadata(&ctx.creds).expect("stat").mode() & 0o777,
            0o600
        );
        let left: Vec<_> = std::fs::read_dir(ctx.creds.parent().expect("dir"))
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(left.len(), 1, "no temp or backup file is left: {left:?}");
    }

    #[test]
    fn creds_file_write_says_why_when_it_cannot_start() {
        let home = TestStore::new(&[]);
        let mut ctx = Ctx::at(home.to_path_buf(), true).unwrap_or_else(|_| panic!("a context"));
        ctx.darwin = false;

        // The credentials directory cannot be made: its parent is a file.
        let blocker = home.join("blocker");
        std::fs::write(&blocker, "").expect("a file");
        ctx.creds = blocker.join("dir").join(".credentials.json");
        let why = ctx
            .creds_file_write(r#"{"a":1}"#)
            .expect_err("no directory can be made");
        assert!(why.starts_with("cannot create "), "{why}");
        assert!(why.contains("blocker/dir"), "{why}");
        assert!(
            why.len() > "cannot create : ".len() + 20,
            "a cause follows: {why}"
        );

        // The rollback copy cannot be made: the name is too long for one more suffix.
        let dir = home.join("long");
        std::fs::create_dir(&dir).expect("a directory");
        ctx.creds = dir.join("c".repeat(250));
        let why = ctx
            .creds_file_write(r#"{"a":1}"#)
            .expect_err("the rollback name is too long");
        assert!(why.starts_with("cannot make a rollback copy of "), "{why}");
        assert!(why.contains("File name too long"), "{why}");

        // The live file cannot be read for its copy: it is a directory.
        ctx.creds = dir.join(".credentials.json");
        std::fs::create_dir(&ctx.creds).expect("a directory in the file's place");
        let why = ctx
            .creds_file_write(r#"{"a":1}"#)
            .expect_err("a directory cannot be copied");
        assert!(why.starts_with("cannot copy "), "{why}");
        assert!(why.contains(".credentials.json to "), "{why}");
        let left: Vec<_> = std::fs::read_dir(&dir)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(left.len(), 1, "the rollback file is removed: {left:?}");
    }

    /// The context for a test that runs against the real `security`: the macOS
    /// code path, the throwaway keychain that CI creates and names in the test
    /// hook variables, and nothing else. It refuses any other keychain, so such a
    /// test can never touch the login keychain.
    ///
    /// The keychain tests share one item in the throwaway keychain, and the tests
    /// run on several threads: each holds the lock for its whole length and
    /// starts with the item deleted. Fields drop in order, so the context and
    /// the directory go before the lock is released.
    struct Throwaway {
        ctx: Ctx,
        _home: TestStore,
        _lock: MutexGuard<'static, ()>,
    }

    fn throwaway_keychain_ctx() -> Throwaway {
        static LOCK: Mutex<()> = Mutex::new(());
        let lock = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(
            std::env::var("SHARKSWAP_TEST_HOOKS").as_deref(),
            Ok("1"),
            "refusing to run without the test hooks"
        );
        let keychain = std::env::var("SHARKSWAP_TEST_KEYCHAIN").expect("a test keychain");
        let name = std::path::Path::new(&keychain)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        assert!(
            name.starts_with("sharkswap-ci")
                && (name.ends_with(".keychain") || name.ends_with(".keychain-db")),
            "not a throwaway keychain: {keychain}"
        );
        let service = std::env::var("SHARKSWAP_TEST_SERVICE").expect("a test service");
        assert!(!service.is_empty() && service != KEYCHAIN_SVC);
        assert!(
            std::path::Path::new(&keychain).is_file(),
            "no keychain file at {keychain}"
        );
        // Delete exactly the item the code under test reads and writes. No item
        // yet is fine: `security` then exits non-zero, which is ignored.
        let t = KeychainTarget::from_env();
        assert_eq!(
            (t.service.as_str(), t.keychain.as_deref()),
            (service.as_str(), Some(keychain.as_str()))
        );
        let _ = Command::new(security_program())
            .args(["delete-generic-password", "-s", &t.service])
            .args(&t.keychain)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let home = TestStore::new(&[]);
        let mut ctx = Ctx::at(home.to_path_buf(), true).unwrap_or_else(|_| panic!("a context"));
        ctx.darwin = true;
        Throwaway {
            ctx,
            _home: home,
            _lock: lock,
        }
    }

    /// Round trip through a real keychain, with the real `security`. It runs only
    /// on a Mac and only when asked (`cargo test -- --ignored`), against the
    /// throwaway keychain that CI creates and names in the test hook variables;
    /// without them it refuses, so it can never touch the login keychain.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "needs a throwaway keychain; run by the macOS CI step"]
    fn keychain_round_trip_through_security_i() {
        let throwaway = throwaway_keychain_ctx();
        let ctx = &throwaway.ctx;
        assert_eq!(ctx.creds_read(), "", "nothing stored yet");
        let tricky = [
            json!({"claudeAiOauth": {"accessToken": "sk-ant-oat01-q\"uote \\ back 'single' sp ace",
                "refreshToken": "sk-ant-ort01-{x}", "expiresAt": 1_900_000_000_000_u64}})
            .to_string(),
            json!({"claudeAiOauth": {"accessToken": "caf\u{e9} \u{1f511} \\\\ \"\"", "expiresAt": 1}, "k": "\\"})
                .to_string(),
        ];
        for json in &tricky {
            ctx.creds_write(json)
                .unwrap_or_else(|_| panic!("write {json}"));
            assert_eq!(&ctx.creds_read(), json, "read back what was written");
        }
        let long = json!({"k": "x".repeat(SECURITY_LINE_MAX)}).to_string();
        ctx.creds_write(&long)
            .unwrap_or_else(|_| panic!("the argv form stores a long login"));
        assert_eq!(&ctx.creds_read(), &long, "read back the long login");
    }

    /// Credentials JSON whose `security -i` line is exactly `line_len` bytes (the
    /// newline not counted). The padding is lower-case letters, which the quoting
    /// leaves alone, in an order that does not repeat, so that no stretch of it
    /// can turn up on stderr by chance.
    fn credentials_for_line_len(t: &KeychainTarget, account: &str, line_len: usize) -> String {
        let wrap = |pad: &str| json!({"claudeAiOauth": {"accessToken": pad}}).to_string();
        let overhead = line_of(t, account, &wrap("")).len() - 1;
        let pad: String = (0..line_len - overhead)
            .map(|i| char::from(b'a' + ((i * 31 + i / 7) % 26) as u8))
            .collect();
        wrap(&pad)
    }

    /// Feed `line` to `program -i` as `security_store` does, and give back
    /// whether it succeeded and what it wrote on stderr (which that function
    /// reports only when it fails).
    fn run_line(program: &str, line: &str) -> (bool, String) {
        let mut child = security_spawn(program, &["-i".to_string()], true).expect("spawned");
        child
            .stdin
            .take()
            .expect("a stdin pipe")
            .write_all(line.as_bytes())
            .expect("sent");
        let out = child.wait_with_output().expect("waited for");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// Fails when `stderr` holds any 12-byte stretch of `secret` (ASCII).
    fn assert_no_fragment(stderr: &str, secret: &str) {
        for window in secret.as_bytes().windows(12) {
            let fragment = std::str::from_utf8(window).expect("ascii");
            assert!(
                !stderr.contains(fragment),
                "stderr repeats part of the credentials"
            );
        }
    }

    #[test]
    fn the_boundary_credentials_make_a_line_of_the_limit_and_a_line_one_over() {
        for keychain in [None, Some("/tmp/kc dir/sharkswap-ci.keychain".to_string())] {
            let t = KeychainTarget {
                service: "sharkswap-ci-test-item".into(),
                keychain,
            };
            let fits = credentials_for_line_len(&t, "runner", SECURITY_LINE_MAX);
            let over = credentials_for_line_len(&t, "runner", SECURITY_LINE_MAX + 1);
            assert!(serde_json::from_str::<Value>(&fits).is_ok());
            assert_eq!(line_of(&t, "runner", &fits).len() - 1, SECURITY_LINE_MAX);
            assert_eq!(argv_of(&t, "runner", &over)[7], over);
        }
    }

    #[test]
    fn a_line_at_the_limit_reaches_security_whole_with_nothing_on_stderr() {
        let _spawn = spawn_lock();
        let dir = TestStore::new(&[]);
        let stdin_file = dir.join("stdin");
        let program = stub_security(&dir, &format!("cat > '{}'", stdin_file.display()));
        let fits = credentials_for_line_len(&target(), "u", SECURITY_LINE_MAX);
        let line = line_of(&target(), "u", &fits);
        let (ok, stderr) = run_line(&program, &line);
        assert!(ok);
        assert_eq!(stderr, "");
        assert_eq!(std::fs::read_to_string(&stdin_file).expect("stdin"), line);
        assert_no_fragment(&stderr, &fits);
    }

    #[test]
    fn the_fragment_check_catches_an_echo_of_the_credentials() {
        let secret = credentials_for_line_len(&target(), "u", SECURITY_LINE_MAX);
        assert_no_fragment("", &secret);
        assert_no_fragment("security: some unrelated complaint", &secret);
        let echoed = format!("unknown command: {}", &secret[100..140]);
        assert!(std::panic::catch_unwind(|| assert_no_fragment(&echoed, &secret)).is_err());
    }

    #[test]
    fn a_password_printed_as_hex_is_decoded_and_plain_text_is_kept() {
        let json = "{\"k\":\"caf\u{e9} \u{1f511}\",\"t\":\"a\tb\"}";
        let hex = |s: &str| s.bytes().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(
            security_password(format!("{}\n", hex(json)).as_bytes()),
            json
        );
        // With no newline after it, and with one that is part of the password:
        // only the newline `security` adds goes, before the hex is read.
        assert_eq!(security_password(hex(json).as_bytes()), json);
        let nl = format!("{json}\n");
        assert_eq!(security_password(format!("{}\n", hex(&nl)).as_bytes()), nl);
        // Printed as it is: printable ASCII, even when it looks like hex, and
        // non-ASCII text from a `security` that took it for printable.
        for plain in [
            "{\"a\":1}",
            "7b7d",
            "deadbeef",
            "0a",
            "c3a9",
            "abc",
            "",
            "{\"k\":\"caf\u{e9}\"}",
        ] {
            assert_eq!(security_password(format!("{plain}\n").as_bytes()), plain);
        }
        // Not decoded: an odd length, bytes that are not UTF-8, text that is not
        // a JSON object, and upper-case digits (`security` prints `%02x`).
        let odd = format!("{}0", hex(json));
        for kept in [odd.as_str(), "ff00", "c3a9", "E9"] {
            assert_eq!(security_password(format!("{kept}\n").as_bytes()), kept);
        }
    }

    /// The edge of the line limit against the real `security`, on the throwaway
    /// keychain: a line of exactly 4094 bytes goes on stdin, one of 4095 takes
    /// the argv form, and each is read back byte for byte. The 4094-byte line is
    /// fed to `security -i` here (as `security_store` does) so that its stderr can
    /// be read: `security -i` takes a longer line in two reads and answers the
    /// second with an error that quotes it, so any stderr that repeats part of the
    /// credentials means the line was cut. It is not limited to a Mac so that it is
    /// compiled and linted everywhere, but it means something only against the
    /// real tool.
    #[test]
    #[ignore = "needs a throwaway keychain; run by the macOS CI step"]
    fn keychain_line_limit_boundary_through_security_i() {
        let throwaway = throwaway_keychain_ctx();
        let ctx = &throwaway.ctx;
        let t = KeychainTarget::from_env();
        let user = std::env::var("USER").unwrap_or_default();
        let fits = credentials_for_line_len(&t, &user, SECURITY_LINE_MAX);
        let over = credentials_for_line_len(&t, &user, SECURITY_LINE_MAX + 1);

        let Ok(SecurityWrite::Line(line)) = security_write(&t, &user, &fits) else {
            panic!("a line of {SECURITY_LINE_MAX} bytes must take the stdin form");
        };
        assert_eq!(line.len() - 1, SECURITY_LINE_MAX);
        let (ok, stderr) = run_line(&security_program(), &line);
        assert!(
            ok,
            "security -i refused the line: {}",
            stderr_excerpt(stderr.as_bytes())
        );
        assert_no_fragment(&stderr, &fits);
        assert_eq!(ctx.creds_read(), fits, "read back the 4094-byte line");

        assert!(
            matches!(security_write(&t, &user, &over), Ok(SecurityWrite::Argv(_))),
            "a line of {} bytes must take the argv form",
            SECURITY_LINE_MAX + 1
        );
        ctx.creds_write(&over)
            .unwrap_or_else(|_| panic!("the argv form stores the 4095-byte line"));
        assert_eq!(ctx.creds_read(), over, "read back the 4095-byte line");
    }

    #[test]
    fn help_and_version_words_are_not_aliases() {
        for word in ["help", "-h", "--help", "--version", "-V"] {
            assert!(
                !alias_ok(word, false),
                "{word} must not be saved as an alias"
            );
            assert!(alias_ok(word, true), "{word} must stay removable");
        }
        assert!(alias_ok("version", false));
        assert!(alias_ok("V", false));
    }
}
