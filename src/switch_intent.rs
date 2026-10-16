//! The switch-intent marker. A global account switch changes three live files
//! (the credentials, then each configuration file), and no call makes them one
//! step. Before the first of them changes, the switch writes
//! `<store>/.identity.lock/switch-pending` (the alias it is about to activate,
//! never a secret) and removes it only when every write has landed or every
//! put-back has. While the file exists, the live identity is uncertain: nothing
//! trusts it, and the identity lock is kept. Only its existence matters, so it
//! is looked at with `lstat` and never opened or parsed, which also makes a
//! dangling link or a directory in its place count.
//!
//! A marker means one of two things. The switch may still be running: the
//! process that holds the lock (the `pid=<N> started=<ISO>` line of the lock's
//! `owner` file) is alive, and the marker goes away when it finishes. Or the
//! switch was cut short: that process is gone, and only a person can say which
//! login is live. [`state`] tells them apart, so a command that meets a running
//! switch says "try again" instead of sending the reader to the lock.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::util::{read_text, shown_line};

const MARKER: &str = "switch-pending";

/// The identity lock directory of `store`.
pub fn lock_dir(store: &Path) -> PathBuf {
    store.join(".identity.lock")
}

/// The marker inside it.
pub fn marker(store: &Path) -> PathBuf {
    lock_dir(store).join(MARKER)
}

/// Whether the lock directory `dir` holds the marker. `lstat` only.
pub fn pending_at(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir.join(MARKER)).is_ok()
}

/// Whether an unfinished switch is recorded in `store`.
pub fn pending(store: &Path) -> bool {
    pending_at(&lock_dir(store))
}

/// What the marker of a store says.
#[derive(Debug, PartialEq)]
pub enum Pending {
    /// No marker.
    Clear,
    /// A marker, and the lock's owner (the text of its owner file, one line) is
    /// a live process: a switch is running.
    InProgress(String),
    /// A marker, and no live owner: the switch was cut short.
    Interrupted,
}

/// Whether `store` holds a marker, and if so whether its switch is running. The
/// owner file is read (bounded, as every store file is) only when there is a
/// marker; the marker itself is still never opened.
///
/// A pid can be reused after its process died, which makes a dead holder look
/// alive. That turns "unfinished account switch" into "in progress", so the
/// command is still refused and the lock still kept (a waiter then runs into
/// the lock timeout); it never turns an interrupted switch into a usable one. A
/// zombie that was not reaped looks alive the same way.
pub fn state(store: &Path) -> Pending {
    state_with(store, || {
        let text = read_text(&lock_dir(store).join("owner")).unwrap_or_default();
        text.lines().next().unwrap_or("").to_string()
    })
}

/// [`state`], with the first line of the owner file read by `read_owner`.
///
/// The owner is read again after the liveness check. A lock that changed hands
/// in between (one switch ended and the next began, so a marker is there either
/// way) is looked at afresh, a few times; one that never holds still counts as
/// cut short, the answer that refuses, unless the marker is gone by then.
fn state_with(store: &Path, mut read_owner: impl FnMut() -> String) -> Pending {
    for _ in 0..3 {
        if !pending(store) {
            return Pending::Clear;
        }
        let owner = read_owner();
        let live = owner_pid(&owner).is_some_and(holder_alive);
        // A switch that ended while the owner was read leaves no marker.
        if !pending(store) {
            return Pending::Clear;
        }
        if read_owner() != owner {
            continue;
        }
        return if live {
            Pending::InProgress(shown_line(&owner))
        } else {
            Pending::Interrupted
        };
    }
    // The last try saw the owner change again; the switch may have ended in it.
    if pending(store) {
        Pending::Interrupted
    } else {
        Pending::Clear
    }
}

/// The pid of an owner line (`pid=<N> started=<ISO>`); none that is not a
/// positive whole number that a pid can be (zero and negative values would mean
/// a process group to `kill`).
fn owner_pid(owner: &str) -> Option<i32> {
    let rest = owner.strip_prefix("pid=")?;
    let digits = rest.split(|c: char| !c.is_ascii_digit()).next()?;
    digits.parse::<i32>().ok().filter(|p| *p > 0)
}

/// Whether process `pid` exists (signal 0 delivers nothing; "not permitted"
/// still means it is there). This process counts only while it holds the lock
/// itself: a marker that names our own pid, with no lock held, is a dead run's.
fn holder_alive(pid: i32) -> bool {
    if u32::try_from(pid).is_ok_and(|p| p == std::process::id()) {
        return crate::store::lock_held_here();
    }
    // SAFETY: signal 0 only checks that the process exists.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The refusal for a switch that is running.
pub fn in_progress_text(owner: &str) -> String {
    format!("an account switch is in progress (owner: {owner}); try again when it finishes")
}

/// The refusal for a switch that was cut short (`to` names the target of a
/// switch the caller made, `doing` what it was about to do).
pub fn unfinished_text(store: &Path, to: &str, doing: &str) -> String {
    format!(
        "unfinished account switch{to}; inspect the identity lock at {} before {doing}",
        lock_dir(store).display()
    )
}

/// Refuse (and say so) while a switch is running or was cut short.
pub fn ready(store: &Path) -> Result<(), i32> {
    match state(store) {
        Pending::Clear => Ok(()),
        Pending::InProgress(owner) => {
            eprintln!("error: {}", in_progress_text(&owner));
            Err(1)
        }
        Pending::Interrupted => refuse_unfinished(store),
    }
}

fn refuse_unfinished(store: &Path) -> Result<(), i32> {
    eprintln!(
        "error: {}",
        unfinished_text(store, "", "using the live login")
    );
    Err(1)
}

/// Like [`ready`], but a running switch is not refused: the caller waits for
/// the lock it holds. Only a switch that was cut short stops the caller.
pub fn ready_to_wait(store: &Path) -> Result<(), i32> {
    match state(store) {
        Pending::Interrupted => refuse_unfinished(store),
        Pending::Clear | Pending::InProgress(_) => Ok(()),
    }
}

/// Record that a switch to `alias` is about to change the live login. The lock
/// must be held. A marker that could only be half made is taken away again.
pub fn publish(store: &Path, alias: &str) -> std::io::Result<()> {
    if fault("publish") {
        return Err(std::io::Error::other("switch fault"));
    }
    let path = marker(store);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    if let Err(e) = file.write_all(format!("{alias}\n").as_bytes()) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    fault("intent");
    Ok(())
}

/// Remove the marker: the switch is finished, or fully put back.
pub fn clear(store: &Path) -> std::io::Result<()> {
    if fault("clear") {
        return Err(std::io::Error::other("switch fault"));
    }
    match std::fs::remove_file(marker(store)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

// The switch-fault test hook (see store.rs): `SHARKSWAP_TEST_SWITCH_FAULT` is a
// comma-separated list of `point:action`. A point is a place in a switch where
// it can be interrupted; the action is `TERM` or `KILL` (the process signals
// itself there) or `fail` (the step reports an error). The points are
//   publish          before the marker is written          (fail: it cannot be)
//   intent           just after the marker is written
//   creds            just after the credentials are written
//   primary          just after ~/.claude.json is written  (fail: it was not)
//   secondary        just after ~/.claude/.claude.json is written (fail likewise)
//   rollback         before a configuration file is put back (fail: it is not)
//   clear            before the marker is removed          (fail: it is not)
/// Whether the step at `point` must fail. A signal action does not return.
#[cfg(any(test, feature = "test-hooks"))]
pub fn fault(point: &str) -> bool {
    let Some(spec) = crate::store::hook(crate::store::HOOK_SWITCH_FAULT) else {
        return false;
    };
    match fault_action(&spec, point) {
        Some(Fault::Fail) => true,
        Some(Fault::Term) => {
            // SAFETY: raising a signal on this process.
            unsafe { libc::raise(libc::SIGTERM) };
            false
        }
        Some(Fault::Kill) => {
            // SAFETY: raising a signal on this process.
            unsafe { libc::raise(libc::SIGKILL) };
            false
        }
        None => false,
    }
}

#[cfg(not(any(test, feature = "test-hooks")))]
pub fn fault(_point: &str) -> bool {
    false
}

#[cfg(any(test, feature = "test-hooks"))]
#[derive(Debug, PartialEq, Clone, Copy)]
enum Fault {
    Term,
    Kill,
    Fail,
}

/// The action `spec` names for `point`, if any.
#[cfg(any(test, feature = "test-hooks"))]
fn fault_action(spec: &str, point: &str) -> Option<Fault> {
    spec.split(',').find_map(|item| {
        let (p, action) = item.split_once(':')?;
        if p.trim() != point {
            return None;
        }
        match action.trim() {
            "TERM" => Some(Fault::Term),
            "KILL" => Some(Fault::Kill),
            "fail" => Some(Fault::Fail),
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{TestStore, end, lock_of_running_switch, signal_lock, sleeper};
    use std::os::unix::fs::{MetadataExt, symlink};

    fn store_with_lock() -> TestStore {
        let store = TestStore::new(&[]);
        std::fs::create_dir(lock_dir(&store)).expect("a lock directory");
        store
    }

    #[test]
    fn no_marker_means_nothing_is_pending() {
        let store = TestStore::new(&[]);
        assert!(!pending(&store), "no lock directory at all");
        assert!(ready(&store).is_ok());
        std::fs::create_dir(lock_dir(&store)).expect("a lock directory");
        assert!(!pending(&store), "a lock directory without the marker");
        assert!(ready(&store).is_ok());
    }

    #[test]
    fn publishing_writes_the_alias_privately_and_clearing_removes_it() {
        let store = store_with_lock();
        publish(&store, "work").expect("publish");
        assert!(pending(&store));
        assert!(ready(&store).is_err());
        let path = marker(&store);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "work\n");
        assert_eq!(
            std::fs::metadata(&path).expect("stat").mode() & 0o777,
            0o600
        );
        // The marker is created, never replaced: a second publish does not overwrite.
        assert!(publish(&store, "other").is_err());
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "work\n");
        clear(&store).expect("clear");
        assert!(!pending(&store));
        clear(&store).expect("clearing what is not there is fine");
    }

    #[test]
    fn any_entry_in_the_marker_place_counts_without_being_opened() {
        // An empty file, a directory and a dangling link: none is read, all count.
        let store = store_with_lock();
        let path = marker(&store);
        std::fs::write(&path, "").expect("an empty marker");
        assert!(pending(&store), "an empty file");
        std::fs::remove_file(&path).expect("rm");

        std::fs::create_dir(&path).expect("a directory marker");
        assert!(pending(&store), "a directory");
        assert!(
            clear(&store).is_err(),
            "a directory is not cleared by a file removal"
        );
        assert!(pending(&store));
        std::fs::remove_dir(&path).expect("rmdir");

        symlink(store.join("nowhere"), &path).expect("a dangling link");
        assert!(!path.exists(), "the link really dangles");
        assert!(pending(&store), "a dangling link");
        assert!(ready(&store).is_err());
        clear(&store).expect("a link is removed as itself");
        assert!(!pending(&store));
    }

    #[test]
    fn an_owner_line_gives_a_pid_only_when_it_can_be_one() {
        assert_eq!(
            owner_pid("pid=4242 started=2026-01-01T00:00:00Z"),
            Some(4242)
        );
        assert_eq!(owner_pid("pid=7"), Some(7));
        for bad in [
            "",
            "pid=",
            "pid=0",
            "pid=-1",
            "pid=-4242",
            "pid=x1",
            "4242",
            " pid=4242",
            "pid=99999999999",
            "pid=2147483648",
        ] {
            assert_eq!(owner_pid(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_state_tells_a_running_switch_from_a_cut_short_one() {
        let _serial = signal_lock();
        let store = TestStore::new(&[]);
        assert_eq!(state(&store), Pending::Clear, "no lock at all");
        std::fs::create_dir(lock_dir(&store)).expect("a lock directory");
        assert_eq!(state(&store), Pending::Clear, "no marker");

        // A marker with no owner file, or one that names no process.
        publish(&store, "work").expect("publish");
        assert_eq!(state(&store), Pending::Interrupted, "no owner file");
        let owner = lock_dir(&store).join("owner");
        for text in ["", "garbage\n", "pid=0 started=x\n", "pid=-1 started=x\n"] {
            std::fs::write(&owner, text).expect("an owner file");
            assert_eq!(state(&store), Pending::Interrupted, "owner {text:?}");
            assert!(ready(&store).is_err());
            assert!(ready_to_wait(&store).is_err());
        }

        // A holder that is alive.
        let holder = sleeper();
        let line = format!("pid={} started=2026-01-01T00:00:00Z", holder.id());
        std::fs::write(&owner, format!("{line}\n")).expect("an owner file");
        assert_eq!(state(&store), Pending::InProgress(line.clone()));
        assert!(ready(&store).is_err(), "the other commands are refused");
        assert!(ready_to_wait(&store).is_ok(), "a waiter is not");

        // The same holder, dead and collected.
        end(holder);
        assert_eq!(state(&store), Pending::Interrupted, "the holder is gone");
        assert!(ready_to_wait(&store).is_err());

        // Our own pid counts only while this process holds the lock itself.
        std::fs::write(
            &owner,
            format!("pid={} started=2026-01-01T00:00:00Z\n", std::process::id()),
        )
        .expect("an owner file");
        assert!(!crate::store::lock_held_here());
        assert_eq!(state(&store), Pending::Interrupted, "a dead run's marker");

        // The marker goes away: nothing is pending, whatever the owner says.
        clear(&store).expect("clear");
        assert_eq!(state(&store), Pending::Clear);
        assert!(ready(&store).is_ok());
    }

    #[test]
    fn a_lock_that_changes_hands_during_the_look_is_looked_at_again() {
        let _serial = signal_lock();
        let store = TestStore::new(&[]);
        let holder = sleeper();
        let (dead, live) = (
            "pid=2147483647 started=2026-01-01T00:00:00Z".to_string(),
            format!("pid={} started=2026-01-01T00:00:05Z", holder.id()),
        );
        std::fs::create_dir(lock_dir(&store)).expect("a lock directory");
        publish(&store, "work").expect("publish");

        // One switch ended and the next began between the two reads of the owner:
        // the first read is the old, dead owner; every later one is the new, live one.
        let mut reads = vec![dead.clone(), live.clone(), live.clone(), live.clone()].into_iter();
        let seen = state_with(&store, || reads.next().expect("no more reads than that"));
        assert_eq!(seen, Pending::InProgress(live.clone()));

        // An owner that is never the same twice cannot be vouched for: fail closed.
        let mut n = 0;
        let seen = state_with(&store, || {
            n += 1;
            format!("pid={} started={n}", holder.id())
        });
        assert_eq!(seen, Pending::Interrupted);
        assert!(n <= 8, "the retries are bounded: {n} reads");

        // The switch ended during the last try: the marker is gone by the final
        // read, so nothing is pending, and nothing may be called unfinished.
        let mut n = 0;
        let seen = state_with(&store, || {
            n += 1;
            if n == 6 {
                clear(&store).expect("the switch ends");
            }
            format!("pid={} started={n}", holder.id())
        });
        assert_eq!(seen, Pending::Clear);
        assert_eq!(n, 6, "three tries, two reads each");
        end(holder);
    }

    #[test]
    fn the_owner_shown_for_a_running_switch_has_no_control_characters() {
        let _serial = signal_lock();
        let store = TestStore::new(&[]);
        let holder = sleeper();
        lock_of_running_switch(&store, holder.id());
        let owner = lock_dir(&store).join("owner");
        std::fs::write(
            &owner,
            format!("pid={} \x1b[31mred\x07\nsecond line\n", holder.id()),
        )
        .expect("an owner file");
        let Pending::InProgress(shown) = state(&store) else {
            panic!("the holder is alive");
        };
        assert_eq!(shown, format!("pid={} [31mred", holder.id()));
        end(holder);
    }

    #[test]
    fn the_owner_shown_for_a_running_switch_is_cut_short() {
        let _serial = signal_lock();
        let store = TestStore::new(&[]);
        let holder = sleeper();
        lock_of_running_switch(&store, holder.id());
        let owner = lock_dir(&store).join("owner");
        std::fs::write(
            &owner,
            format!("pid={} {}\n", holder.id(), "x".repeat(1 << 20)),
        )
        .expect("an owner file");
        let Pending::InProgress(shown) = state(&store) else {
            panic!("the holder is alive");
        };
        assert!(shown.starts_with(&format!("pid={} xxx", holder.id())));
        assert!(shown.ends_with("xxx..."), "the cut is marked");
        assert_eq!(shown.len(), 123);
        end(holder);
    }

    #[test]
    fn the_refusals_say_what_the_reader_can_do() {
        let store = Path::new("/tmp/store");
        let running = in_progress_text("pid=7 started=2026-01-01T00:00:00Z");
        assert_eq!(
            running,
            "an account switch is in progress (owner: pid=7 started=2026-01-01T00:00:00Z); try again when it finishes"
        );
        assert!(!running.contains("inspect") && !running.contains("rm "));
        assert_eq!(
            unfinished_text(store, " to work", "rotating"),
            "unfinished account switch to work; inspect the identity lock at /tmp/store/.identity.lock before rotating"
        );
    }

    #[test]
    fn the_fault_spec_picks_the_action_for_its_point() {
        let spec = "creds:KILL, primary:TERM,clear:fail,odd:nonsense,noaction";
        assert_eq!(fault_action(spec, "creds"), Some(Fault::Kill));
        assert_eq!(fault_action(spec, "primary"), Some(Fault::Term));
        assert_eq!(fault_action(spec, "clear"), Some(Fault::Fail));
        assert_eq!(fault_action(spec, "odd"), None, "an unknown action");
        assert_eq!(fault_action(spec, "noaction"), None);
        assert_eq!(fault_action(spec, "secondary"), None, "a point not named");
        assert_eq!(fault_action("", "creds"), None);
    }

    #[test]
    fn without_the_test_flag_no_fault_fires() {
        // The unit tests do not set SHARKSWAP_TEST_HOOKS, so this must be inert.
        for point in [
            "publish",
            "intent",
            "creds",
            "primary",
            "secondary",
            "rollback",
            "clear",
        ] {
            assert!(!fault(point), "{point}");
        }
    }
}
