//! Fixtures for the unit tests: a throwaway store directory and probe readings
//! stamped at a fixed instant. Nothing here touches a real home directory.

use crate::avail::Avail;
use crate::util::mktemp_dir;
use serde_json::{Value, json};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, UNIX_EPOCH};

/// Signal dispositions belong to the whole process, and the tests run on
/// several threads. A test that installs or looks at a handler (`catch_signals`,
/// `IdLock`) holds this guard for its whole length.
pub fn signal_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    // A test that failed while holding it must not fail every test after it.
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A test that writes a script and runs it holds this guard until the run is over.
/// Another test's `fork` between the write and the `exec` would hand the child the
/// script's still-open write descriptor, and the `exec` would fail with "Text
/// file busy".
pub fn spawn_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What signal `sig` does now: SIG_DFL, SIG_IGN or a handler's address.
pub fn disposition(sig: libc::c_int) -> libc::sighandler_t {
    // SAFETY: querying a disposition with a live, zeroed sigaction.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        assert_eq!(libc::sigaction(sig, std::ptr::null(), &mut old), 0);
        old.sa_sigaction
    }
}

pub const H: f64 = 3_600_000.0;
/// A fixed instant, in ms.
pub const NOW: f64 = 1_800_000_000_000.0;

pub fn sec(ms: f64) -> i64 {
    (ms / 1000.0).floor() as i64
}

/// A store directory under the system temp dir, removed when dropped. A
/// `Value::String` body is written as it is (so a test can write broken JSON);
/// any other value is written as JSON. Every file's mtime is `NOW`, because file
/// age is judged against the fixed clock.
pub struct TestStore {
    dir: PathBuf,
}

impl TestStore {
    pub fn new(files: &[(&str, Value)]) -> TestStore {
        let prefix = format!("{}/cs-unit-", std::env::temp_dir().display());
        let dir = mktemp_dir(&prefix).expect("a temp directory");
        for (name, body) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().expect("a parent directory"))
                .expect("a subdirectory");
            let text = match body {
                Value::String(raw) => raw.clone(),
                other => other.to_string(),
            };
            std::fs::write(&path, text).expect("a store file");
            touch(&path, NOW);
        }
        TestStore { dir }
    }
}

impl Deref for TestStore {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.dir
    }
}

impl Drop for TestStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Set a file's modification time to `ms` since the epoch.
pub fn touch(path: &Path, ms: f64) {
    let when = UNIX_EPOCH + Duration::from_millis(ms as u64);
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("a file to touch")
        .set_modified(when)
        .expect("an mtime");
}

pub fn meta(email: &str) -> Value {
    json!({ "emailAddress": email, "accountUuid": "00000000-0000-4000-8000-000000000000" })
}

/// An availability entry read at `at_ms`, whose windows reset 2 h and 48 h after `base_ms`.
pub fn reading_at(util5: f64, util7: f64, at_ms: f64, base_ms: f64) -> Value {
    json!({
        "state": "ok", "reason": "ok", "at": sec(at_ms), "via": "headers", "http": 200, "cred": "snapshot",
        "limits": {
            "5h": { "util": util5, "resets": sec(base_ms + 2.0 * H), "status": "allowed" },
            "7d": { "util": util7, "resets": sec(base_ms + 48.0 * H), "status": "allowed" },
        },
    })
}

/// A reading taken a minute before `NOW`.
pub fn reading(util5: f64, util7: f64) -> Value {
    reading_at(util5, util7, NOW - 60_000.0, NOW)
}

pub fn avail(v: Value) -> Avail {
    v.as_object().expect("an object").clone()
}

/// A FIFO at `path`. Nothing writes to it, so a plain open for reading blocks
/// for good.
pub fn make_fifo(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("a path without NUL");
    // SAFETY: `c` is a valid NUL-terminated path for the whole call.
    assert_eq!(
        unsafe { libc::mkfifo(c.as_ptr(), 0o600) },
        0,
        "mkfifo failed"
    );
}

/// Runs `f` on a thread and waits for it at most 10 s, so a reader that blocks
/// on the FIFO at `fifo` fails its test instead of hanging the suite. On a
/// timeout the FIFO is opened for reading and writing, which lets a reader
/// stuck in `open` go on and see end of file, and then the test panics: no
/// thread is left blocked on a file outside the test's temp directory.
pub fn within_deadline<T: Send + 'static>(
    fifo: &Path,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(f());
    });
    match receiver.recv_timeout(Duration::from_secs(10)) {
        Ok(value) => value,
        Err(_) => {
            let _ = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(fifo);
            panic!("the read blocked on the FIFO at {}", fifo.display());
        }
    }
}
