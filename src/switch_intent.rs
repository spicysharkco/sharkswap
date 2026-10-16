//! The switch-intent marker. A global account switch changes three live files
//! (the credentials, then each configuration file), and no call makes them one
//! step. Before the first of them changes, the switch writes
//! `<store>/.identity.lock/switch-pending` (the alias it is about to activate,
//! never a secret) and removes it only when every write has landed or every
//! put-back has. While the file exists, the live identity is uncertain: nothing
//! trusts it, and the identity lock is kept. Only its existence matters, so it
//! is looked at with `lstat` and never opened or parsed, which also makes a
//! dangling link or a directory in its place count.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

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

/// Refuse (and say so) while an unfinished switch is recorded.
pub fn ready(store: &Path) -> Result<(), i32> {
    if pending(store) {
        eprintln!(
            "error: unfinished account switch; inspect the identity lock at {} before using the live login",
            lock_dir(store).display()
        );
        return Err(1);
    }
    Ok(())
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
    use crate::testkit::TestStore;
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
