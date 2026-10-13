//! Small helpers that reproduce the number and time behaviour the account
//! views depend on: rounding, number parsing, fixed-point text, ISO stamps.

use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Round half up (toward +infinity), the way the readings have always been shown.
pub fn js_round(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let f = x.floor();
    if x - f >= 0.5 { f + 1.0 } else { f }
}

/// Lenient number parse: blank is 0, junk is NaN. `STALE_DAYS=" 3 "` is 3.
pub fn js_number(raw: &str) -> f64 {
    let s = raw.trim();
    if s.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            return u64::from_str_radix(rest, radix).map_or(f64::NAN, |n| n as f64);
        }
    }
    match s {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
    {
        return f64::NAN;
    }
    s.parse::<f64>().unwrap_or(f64::NAN)
}

/// Fixed-point text with ties rounded up (Rust itself rounds exact ties to even).
pub fn to_fixed(x: f64, digits: usize) -> String {
    if !x.is_finite() {
        return x.to_string();
    }
    let neg = x < 0.0;
    let a = x.abs();
    let long = format!("{:.*}", digits + 40, a);
    let (int, frac) = long.split_once('.').unwrap_or((&long, ""));
    let keep = &frac[..digits.min(frac.len())];
    let rest = &frac[digits.min(frac.len())..];
    let tie = rest.starts_with('5') && rest[1..].bytes().all(|b| b == b'0');
    let out = if tie {
        let mut n: u128 = format!("{int}{keep}").parse().unwrap_or(0);
        n += 1;
        let s = format!("{n:0>width$}", width = digits + 1);
        let (i, f) = s.split_at(s.len() - digits);
        if digits == 0 {
            i.to_string()
        } else {
            format!("{i}.{f}")
        }
    } else {
        format!("{:.*}", digits, a)
    };
    if neg && out.bytes().any(|b| (b'1'..=b'9').contains(&b)) {
        format!("-{out}")
    } else {
        out
    }
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `2026-10-12T04:27:26.933Z` for an epoch-milliseconds value.
pub fn iso_ms(ms: f64) -> String {
    let ms = ms.trunc() as i64;
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (y, m, d) = civil(days);
    let (h, mi, s, milli) = (
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000,
    );
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z")
}

/// `2026-10-12T04:27:26Z` for epoch seconds.
pub fn iso_secs(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

pub fn wall_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

pub fn wall_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub fn mtime_ms(path: &Path) -> Option<f64> {
    let t = std::fs::metadata(path).ok()?.modified().ok()?;
    let d = t.duration_since(UNIX_EPOCH).ok()?;
    Some(d.as_secs() as f64 * 1000.0 + f64::from(d.subsec_nanos()) / 1e6)
}

/// Whole file as text; bytes that are not UTF-8 become U+FFFD instead of an error.
pub fn read_text(path: &Path) -> std::io::Result<String> {
    Ok(String::from_utf8_lossy(&std::fs::read(path)?).into_owned())
}

pub fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&read_text(path).ok()?).ok()
}

pub fn is_record(v: &Value) -> bool {
    v.is_object()
}

/// A JSON number as f64 (any finite number), nothing else.
pub fn num(v: &Value) -> Option<f64> {
    v.as_f64().filter(|n| n.is_finite())
}

/// `all` ASCII digits and not empty.
pub fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

pub fn env_or_empty(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

/// Names in a directory, sorted bytewise; empty when it cannot be read.
pub fn list_dir(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => return Vec::new(),
    };
    names.sort();
    names
}

/// `strftime` in the machine's zone for epoch seconds; None when the value is
/// outside what the C library can express.
pub fn local_fmt(secs: i64, format: &str) -> Option<String> {
    let t = secs as libc::time_t;
    if t != secs {
        return None;
    }
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `t` and `tm` are valid for the call; localtime_r writes only into `tm`.
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return None;
    }
    let fmt = std::ffi::CString::new(format).ok()?;
    let mut buf = [0u8; 128];
    // SAFETY: `buf` is writable for its length and `fmt` is NUL-terminated.
    let n = unsafe { libc::strftime(buf.as_mut_ptr().cast(), buf.len(), fmt.as_ptr(), &tm) };
    if n == 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[..n]).into_owned())
}

/// `$(...)` strips trailing newlines.
pub fn trim_nl(s: &str) -> &str {
    s.trim_end_matches('\n')
}

/// The last JSON value of a stream (what `jq -e` judges), None when the text
/// holds no value or is not valid JSON.
pub fn last_json(text: &str) -> Option<Value> {
    let mut last = None;
    for v in serde_json::Deserializer::from_str(text).into_iter::<Value>() {
        last = Some(v.ok()?);
    }
    last
}

/// Exactly one JSON value in the text, or None (empty, several, invalid).
pub fn single_json(text: &str) -> Option<Value> {
    let mut it = serde_json::Deserializer::from_str(text).into_iter::<Value>();
    let first = it.next()?.ok()?;
    if it.next().is_some() {
        None
    } else {
        Some(first)
    }
}

/// What `jq -r '<v> // empty'` prints: nothing for null, false or no value.
pub fn jq_text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(true)) => "true".into(),
        Some(other) => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

/// A float as the JSON number jq would print: whole values without a fraction.
pub fn json_number(x: f64) -> Value {
    if x.is_finite() && x.fract() == 0.0 && x.abs() < 9.0e15 {
        Value::from(x as i64)
    } else {
        serde_json::Number::from_f64(x).map_or(Value::Null, Value::Number)
    }
}

/// `x` is a whole number in [0, 2^53 - 1], as the readers require.
pub fn safe_whole(x: f64) -> bool {
    (0.0..=9_007_199_254_740_991.0).contains(&x) && x.floor() == x
}

/// Error text without the "(os error N)" tail.
pub fn os_msg(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `path` with `suffix` appended to its last component, built from the raw
/// OS string so a path that is not valid UTF-8 survives.
pub fn suffixed(path: &Path, suffix: &str) -> OsString {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name
}

/// `mktemp PREFIX` + six random characters: a new empty file, mode 0600.
pub fn mktemp(prefix: impl AsRef<OsStr>) -> std::io::Result<std::path::PathBuf> {
    use std::os::unix::fs::OpenOptionsExt;
    const SET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let mut seed = u64::from(nanos)
        ^ (u64::from(std::process::id()) << 20)
        ^ TEMP_SEQ
            .fetch_add(0x9E37_79B9, std::sync::atomic::Ordering::Relaxed)
            .wrapping_mul(0x2545_F491_4F6C_DD1D);
    for _ in 0..100 {
        let mut name = prefix.as_ref().to_owned();
        let mut tail = String::with_capacity(6);
        for _ in 0..6 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            tail.push(SET[(seed % SET.len() as u64) as usize] as char);
        }
        name.push(&tail);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&name)
        {
            Ok(_) => return Ok(name.into()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not make a unique name",
    ))
}

/// `mktemp -d PREFIXxxxxxx`: a new empty directory, mode 0700.
pub fn mktemp_dir(prefix: impl AsRef<OsStr>) -> std::io::Result<std::path::PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let f = mktemp(prefix)?;
    std::fs::remove_file(&f)?;
    std::fs::DirBuilder::new().mode(0o700).create(&f)?;
    Ok(f)
}

/// Replace `path` with `bytes` so a reader (or a crash, or a power cut) sees
/// either the old file or the whole new one: a temp file in the same
/// directory, written and fsynced at `mode`, renamed over `path`, then the
/// directory is fsynced so the rename itself is durable. The temp name is built
/// from the OS string, so a path that is not UTF-8 works. A symlink at `path`
/// is replaced by a regular file, not followed. On any error the temp file is
/// removed and `path` is left as it was.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
    })?;
    let mut prefix = name.to_owned();
    prefix.push(".tmp.");
    let tmp = mktemp(dir.join(prefix))?;
    let staged = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).open(&tmp)?;
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = staged {
        rm_f(&tmp);
        return Err(e);
    }
    match std::fs::File::open(dir)?.sync_all() {
        // A filesystem that cannot sync a directory has nothing more to give.
        Err(e) if matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOTSUP)) => Ok(()),
        other => other,
    }
}

/// `cp -p`: contents and modification time. The copy is always mode 0600, set
/// before any byte is written, so a backup of a token file never ends up more
/// readable than the store, not even briefly.
pub fn copy_p(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut input = std::fs::File::open(src)?;
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(dst)?;
    out.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    std::io::copy(&mut input, &mut out)?;
    if let Ok(m) = input.metadata().and_then(|m| m.modified()) {
        out.set_modified(m)?;
    }
    Ok(())
}

/// The signals a command cleans up after: terminal hangup, Ctrl-C, Ctrl-\, and
/// a stop. (A stop of the job with Ctrl-Z, SIGTSTP, only pauses it and is left alone.)
const CAUGHT: [libc::c_int; 4] = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM];

/// The dispositions HUP, INT, QUIT and TERM had before `catch_signals`.
pub struct SavedSignals([libc::sigaction; 4]);

/// Install `handler` for HUP, INT, QUIT and TERM, except where the signal is already
/// ignored: `nohup cs watch &` hands the process an ignored SIGHUP on purpose,
/// and a handler there would turn that off. Returns the old dispositions.
pub fn catch_signals(handler: extern "C" fn(libc::c_int)) -> SavedSignals {
    // SAFETY: an all-zero sigaction is a valid value, and every pointer given to
    // sigaction points at a live sigaction.
    unsafe {
        let mut saved: [libc::sigaction; 4] = std::mem::zeroed();
        for (old, sig) in saved.iter_mut().zip(CAUGHT) {
            if libc::sigaction(sig, std::ptr::null(), old) != 0 {
                old.sa_sigaction = libc::SIG_DFL;
            }
            if old.sa_sigaction == libc::SIG_IGN {
                continue;
            }
            let mut new: libc::sigaction = std::mem::zeroed();
            new.sa_sigaction = handler as libc::sighandler_t;
            libc::sigemptyset(&mut new.sa_mask);
            libc::sigaction(sig, &new, std::ptr::null_mut());
        }
        SavedSignals(saved)
    }
}

impl SavedSignals {
    /// Put back exactly the dispositions that `catch_signals` found.
    pub fn restore(&self) {
        for (old, sig) in self.0.iter().zip(CAUGHT) {
            // SAFETY: `old` is a sigaction that the system filled in.
            unsafe { libc::sigaction(sig, old, std::ptr::null_mut()) };
        }
    }
}

/// Bash `[[ -f ]]`: a regular file, symlinks followed.
pub fn is_file(path: &Path) -> bool {
    path.is_file()
}

/// What the shell glob `DIR/*.json` lists: names ending in `.json`, no leading
/// dot, sorted bytewise.
pub fn glob_json(dir: &Path) -> Vec<String> {
    list_dir(dir)
        .into_iter()
        .filter(|n| n.ends_with(".json") && !n.starts_with('.'))
        .collect()
}

/// `jq -e` over a file: the last value, None when the file is missing or invalid.
pub fn read_last_json(path: &Path) -> Option<Value> {
    last_json(&read_text(path).ok()?)
}

/// `rm -f`: errors are ignored.
pub fn rm_f(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{TestStore, disposition, signal_lock};
    #[cfg(not(target_os = "macos"))]
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).expect("metadata").mode() & 0o7777
    }

    fn names(dir: &Path) -> Vec<OsString> {
        let mut v: Vec<OsString> = std::fs::read_dir(dir)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn write_atomic_writes_the_bytes_at_the_mode_and_leaves_no_temp_file() {
        let dir = TestStore::new(&[]);
        let path = dir.join("creds.json");
        write_atomic(&path, b"one\n", 0o600).expect("first write");
        assert_eq!(std::fs::read(&path).expect("read"), b"one\n");
        assert_eq!(mode(&path), 0o600);
        // A longer file, then a shorter one: nothing of the old text is left.
        write_atomic(&path, b"a much longer second text\n", 0o600).expect("second write");
        write_atomic(&path, b"3\n", 0o640).expect("third write");
        assert_eq!(std::fs::read(&path).expect("read"), b"3\n");
        assert_eq!(mode(&path), 0o640);
        assert_eq!(names(&dir), vec![OsString::from("creds.json")]);
    }

    #[test]
    fn write_atomic_replaces_a_loose_file_with_a_tight_one() {
        let dir = TestStore::new(&[]);
        let path = dir.join("loose.json");
        std::fs::write(&path, "old").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        write_atomic(&path, b"new", 0o600).expect("write");
        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::read(&path).expect("read"), b"new");
    }

    #[test]
    fn write_atomic_replaces_a_symlink_and_leaves_its_target_alone() {
        let dir = TestStore::new(&[]);
        let target = dir.join("target.json");
        std::fs::write(&target, "target text").expect("seed");
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        write_atomic(&link, b"new", 0o600).expect("write");
        assert!(!link.is_symlink(), "the link is now a regular file");
        assert_eq!(std::fs::read(&link).expect("read"), b"new");
        assert_eq!(std::fs::read(&target).expect("read"), b"target text");
    }

    // APFS refuses a file name that is not UTF-8 (EILSEQ), so a Mac cannot
    // have such a path in the first place.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn write_atomic_names_its_temp_file_from_the_raw_path() {
        let dir = TestStore::new(&[]);
        let path = dir.join(OsStr::from_bytes(b"cr\xffeds.json"));
        write_atomic(&path, b"x", 0o600).expect("write to a name that is not UTF-8");
        assert_eq!(std::fs::read(&path).expect("read"), b"x");
        assert_eq!(names(&dir).len(), 1);
    }

    #[test]
    fn write_atomic_failure_changes_nothing_and_leaves_no_temp_file() {
        let dir = TestStore::new(&[]);
        let missing = dir.join("no-such-dir").join("f.json");
        assert!(write_atomic(&missing, b"x", 0o600).is_err());
        // A directory where the file should go: the rename fails after the write.
        let blocked = dir.join("blocked.json");
        std::fs::create_dir(&blocked).expect("mkdir");
        std::fs::write(blocked.join("keep"), "k").expect("seed");
        assert!(write_atomic(&blocked, b"x", 0o600).is_err());
        assert_eq!(
            names(&dir),
            vec![OsString::from("blocked.json")],
            "only the directory remains"
        );
        assert!(blocked.join("keep").is_file());
    }

    #[test]
    fn copy_p_makes_a_private_copy_with_the_same_time() {
        let dir = TestStore::new(&[]);
        let src = dir.join("src.json");
        std::fs::write(&src, "token").expect("seed");
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let dst = dir.join("dst.json");
        copy_p(&src, &dst).expect("copy");
        assert_eq!(mode(&dst), 0o600, "a 0644 source gives a 0600 copy");
        assert_eq!(std::fs::read(&dst).expect("read"), b"token");
        assert_eq!(
            std::fs::metadata(&dst).and_then(|m| m.modified()).ok(),
            std::fs::metadata(&src).and_then(|m| m.modified()).ok()
        );
        // Over an existing loose file: the copy ends up private too.
        let old = dir.join("old.json");
        std::fs::write(&old, "a longer old text").expect("seed");
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        copy_p(&src, &old).expect("copy over");
        assert_eq!(mode(&old), 0o600);
        assert_eq!(std::fs::read(&old).expect("read"), b"token");
    }

    // Not on macOS: APFS refuses a file name that is not UTF-8 (EILSEQ).
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn mktemp_takes_a_prefix_that_is_not_utf8() {
        let dir = TestStore::new(&[]);
        let prefix = dir.join(OsStr::from_bytes(b"pre\xfe."));
        let made = mktemp(&prefix).expect("mktemp");
        assert!(
            made.as_os_str()
                .as_bytes()
                .starts_with(prefix.as_os_str().as_bytes())
        );
        assert_eq!(mode(&made), 0o600);
    }

    extern "C" fn marker(_: libc::c_int) {}

    extern "C" fn other_marker(_: libc::c_int) {}

    fn set_disposition(sig: libc::c_int, handler: libc::sighandler_t) {
        // SAFETY: installing a handler value with a live, zeroed sigaction.
        unsafe {
            let mut new: libc::sigaction = std::mem::zeroed();
            new.sa_sigaction = handler;
            libc::sigemptyset(&mut new.sa_mask);
            assert_eq!(libc::sigaction(sig, &new, std::ptr::null_mut()), 0);
        }
    }

    /// Dispositions belong to the whole process, so every test that changes them
    /// holds `signal_lock`.
    #[test]
    fn catch_signals_leaves_an_ignored_signal_alone_and_restores_exactly() {
        let _serial = signal_lock();
        let originals: Vec<libc::sighandler_t> = CAUGHT.iter().map(|s| disposition(*s)).collect();
        let theirs = other_marker as extern "C" fn(libc::c_int) as libc::sighandler_t;
        let ours = marker as extern "C" fn(libc::c_int) as libc::sighandler_t;
        set_disposition(libc::SIGHUP, libc::SIG_IGN);
        set_disposition(libc::SIGINT, theirs);
        set_disposition(libc::SIGQUIT, libc::SIG_DFL);
        set_disposition(libc::SIGTERM, libc::SIG_DFL);

        let saved = catch_signals(marker);
        assert_eq!(
            disposition(libc::SIGHUP),
            libc::SIG_IGN,
            "nohup's ignore stays"
        );
        assert_eq!(disposition(libc::SIGINT), ours);
        assert_eq!(disposition(libc::SIGQUIT), ours);
        assert_eq!(disposition(libc::SIGTERM), ours);

        saved.restore();
        assert_eq!(disposition(libc::SIGHUP), libc::SIG_IGN);
        assert_eq!(disposition(libc::SIGINT), theirs);
        assert_eq!(disposition(libc::SIGQUIT), libc::SIG_DFL);
        assert_eq!(disposition(libc::SIGTERM), libc::SIG_DFL);

        for (sig, old) in CAUGHT.iter().zip(originals) {
            set_disposition(*sig, old);
        }
    }
}
