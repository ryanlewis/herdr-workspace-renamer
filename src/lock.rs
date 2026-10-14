//! Serializing whole sweeps.
//!
//! Overlapping event-triggered processes would race on the read-modify-write
//! of state.json: the last writer drops the other's entries, and the
//! ownership guard then misreads those workspaces as user-named, permanently.
//! So each sweep holds `.lock` in the state directory. The lock's mtime is
//! its liveness signal: a live sweep refreshes it between herdr calls, and a
//! lock older than [`LOCK_STALE_MS`] is from a crashed sweep and may be
//! taken over.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::warn;

const LOCK_STALE_MS: i128 = 30_000;
const RETRY_WINDOW_MS: i128 = 3_000;
const RETRY_DELAY: Duration = Duration::from_millis(50);

/// The filesystem calls the takeover depends on, as a seam so tests can
/// stage the races between them.
pub trait LockFs {
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn write_pid(&self, file: &mut File, pid: &str) -> io::Result<()> {
        file.write_all(pid.as_bytes())
    }
}

pub struct RealFs;

impl LockFs for RealFs {}

fn now_ms() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i128)
}

fn mtime_ms(meta: &Metadata) -> i128 {
    i128::from(meta.mtime()) * 1000 + i128::from(meta.mtime_nsec()) / 1_000_000
}

fn is_stale(meta: &Metadata) -> bool {
    now_ms() - mtime_ms(meta) > LOCK_STALE_MS
}

/// A held sweep lock, released on drop.
#[derive(Debug)]
pub struct Lock {
    path: PathBuf,
    pid: String,
}

impl Lock {
    /// Takes the lock in `dir`, taking over a stale one; `None` when another
    /// sweep holds it.
    pub fn acquire(dir: &Path, fs: &impl LockFs) -> Option<Self> {
        let path = dir.join(".lock");
        let pid = std::process::id().to_string();
        let created = fs::create_dir_all(dir).and_then(|()| create(&path, &pid, fs));
        // Otherwise: lock vanished, unreadable, or another stealer won. Skip
        // this sweep.
        let ours = created.is_ok() || take_over(dir, &path, &pid, fs).unwrap_or(false);
        ours.then_some(Self { path, pid })
    }

    /// Refreshes the lock's mtime so a slow sweep isn't taken for a crashed
    /// one. A lock that's gone (taken over after a stall) is left alone.
    pub fn touch(&self) {
        let now = SystemTime::now();
        if let Ok(file) = File::open(&self.path) {
            let _ = file.set_times(fs::FileTimes::new().set_accessed(now).set_modified(now));
        }
    }
}

impl Drop for Lock {
    /// Removes the lock only if it is still ours: after a stall past
    /// [`LOCK_STALE_MS`] it may have been taken over, and the file now
    /// serializes someone else's sweep.
    fn drop(&mut self) {
        if fs::read_to_string(&self.path).is_ok_and(|owner| owner == self.pid) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Exclusive create, then the pid. If the pid write fails, remove the file:
/// an ownerless lock would block every sweep until it went stale.
fn create(path: &Path, pid: &str, fs: &impl LockFs) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    if let Err(e) = fs.write_pid(&mut file, pid) {
        drop(file);
        fs::remove_file(path)?;
        return Err(e);
    }
    Ok(())
}

/// Takes over a stale lock: move it aside, then contend for the vacated slot
/// with the same exclusive create. The move is not conditional on what it
/// moves: between our stat and the move another stealer may have evicted the
/// stale lock and taken a fresh one, or a stalled owner may have refreshed
/// it. So check that what moved is the same file and still stale (a fresh
/// lock can reuse the stale one's inode number); if not, put it back and
/// back off.
fn take_over(dir: &Path, path: &Path, pid: &str, fs: &impl LockFs) -> io::Result<bool> {
    let stale = fs::metadata(path)?;
    if !is_stale(&stale) {
        return Ok(false);
    }
    let tomb = dir.join(format!(".lock.stale-{pid}"));
    fs.rename(path, &tomb)?;
    let moved = fs::metadata(&tomb)?;
    if moved.dev() != stale.dev() || moved.ino() != stale.ino() || !is_stale(&moved) {
        match fs::hard_link(&tomb, path) {
            // The slot was re-taken meanwhile: nothing to restore into.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            // E.g. no hard links on this filesystem: move it back rather than
            // delete a live lock.
            Err(_) => {
                fs.rename(&tomb, path)?;
                return Ok(false);
            }
            Ok(()) => {}
        }
        fs::remove_file(&tomb)?;
        return Ok(false);
    }
    fs::remove_file(&tomb)?;
    create(path, pid, fs)?;
    Ok(true)
}

/// When the most recent sweep started, in ms since the epoch (0 if never):
/// `.last-sweep`'s mtime.
fn last_sweep_start(dir: &Path) -> f64 {
    fs::metadata(dir.join(".last-sweep")).map_or(0.0, |m| {
        m.mtime() as f64 * 1000.0 + m.mtime_nsec() as f64 / 1_000_000.0
    })
}

fn stamp_sweep_start(dir: &Path) {
    // A failed stamp just means extra sweeps: harmless.
    let _ = fs::create_dir_all(dir).and_then(|()| File::create(dir.join(".last-sweep")));
}

/// Runs `sweep` under the lock in `dir`, unless a sweep that started after
/// this call already covered it.
///
/// An in-flight sweep may have read herdr's state before the change that
/// fired this event, so "a sweep is already running" is not coverage: only
/// a sweep that started after the event arrived is. On contention this
/// waits briefly and re-checks rather than skipping (which would leave a
/// label stale until some unrelated later event), and gives up after
/// [`RETRY_WINDOW_MS`]: herdr events are frequent enough that a later sweep
/// heals a rare miss. A reset is different: only a sweep that carries it can
/// cover it, so it never counts as covered, and it says so if it gives up.
pub fn serialized(dir: &Path, reset: Option<&str>, fs: &impl LockFs, sweep: impl FnOnce(&Lock)) {
    let arrival = now_ms();
    let covered = || reset.is_none() && last_sweep_start(dir) > arrival as f64;
    loop {
        if covered() {
            return;
        }
        if let Some(lock) = Lock::acquire(dir, fs) {
            if !covered() {
                stamp_sweep_start(dir);
                sweep(&lock);
            }
            return;
        }
        if now_ms() - arrival > RETRY_WINDOW_MS {
            if let Some(id) = reset {
                warn(format!(
                    "reset of {id} skipped: another sweep held the lock"
                ));
            }
            return;
        }
        thread::sleep(RETRY_DELAY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TempDir, take_stderr};
    use std::cell::Cell;

    fn name(p: &Path) -> String {
        p.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }

    fn is_takeover(from: &Path, to: &Path) -> bool {
        name(from) == ".lock" && name(to).starts_with(".lock.stale-")
    }

    /// A lock file owned by `owner`, last refreshed `age_ms` ago.
    fn foreign_lock(dir: &Path, owner: &str, age_ms: u64) {
        let path = dir.join(".lock");
        fs::write(&path, owner).unwrap();
        let t = SystemTime::now() - Duration::from_millis(age_ms);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn owner(dir: &Path) -> Option<String> {
        fs::read_to_string(dir.join(".lock")).ok()
    }

    fn sweeps(dir: &Path, reset: Option<&str>, fs: &impl LockFs) -> u32 {
        let mut n = 0;
        serialized(dir, reset, fs, |_| n += 1);
        n
    }

    #[test]
    fn normal_run_releases_the_lock() {
        let dir = TempDir::new();
        let mut held = None;
        serialized(dir.path(), None, &RealFs, |_| held = owner(dir.path()));
        assert_eq!(held, Some(std::process::id().to_string()));
        assert_eq!(owner(dir.path()), None);
    }

    #[test]
    fn fresh_lock_held_by_another_sweep_skips_and_is_kept() {
        let dir = TempDir::new();
        foreign_lock(dir.path(), "99999", 0);
        assert_eq!(sweeps(dir.path(), None, &RealFs), 0);
        assert_eq!(owner(dir.path()).as_deref(), Some("99999"));
    }

    #[test]
    fn stale_lock_taken_over() {
        let dir = TempDir::new();
        foreign_lock(dir.path(), "99999", 60_000);
        assert_eq!(sweeps(dir.path(), None, &RealFs), 1);
        assert_eq!(owner(dir.path()), None);
    }

    /// Another stealer evicts the stale lock and takes a fresh one just
    /// before our takeover move.
    struct StealRace(Cell<bool>);

    impl LockFs for StealRace {
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            if !self.0.replace(true) && is_takeover(from, to) {
                let other = from.with_file_name(".lock.stale-other");
                fs::rename(from, &other)?;
                fs::remove_file(&other)?;
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(from)?
                    .write_all(b"88888")?;
            }
            fs::rename(from, to)
        }
    }

    #[test]
    fn stale_lock_retaken_mid_takeover_backs_off() {
        let dir = TempDir::new();
        foreign_lock(dir.path(), "99999", 60_000);
        assert_eq!(sweeps(dir.path(), None, &StealRace(Cell::new(false))), 0);
        assert_eq!(
            owner(dir.path()).as_deref(),
            Some("88888"),
            "the other stealer's fresh lock must be put back"
        );
    }

    /// The stale lock's owner refreshes it just before our takeover move.
    struct StealTouch(Cell<bool>);

    impl LockFs for StealTouch {
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            if !self.0.replace(true) && is_takeover(from, to) {
                File::options()
                    .write(true)
                    .open(from)?
                    .set_modified(SystemTime::now())?;
            }
            fs::rename(from, to)
        }
    }

    #[test]
    fn stale_lock_refreshed_mid_takeover_backs_off() {
        let dir = TempDir::new();
        foreign_lock(dir.path(), "99999", 60_000);
        assert_eq!(sweeps(dir.path(), None, &StealTouch(Cell::new(false))), 0);
        assert_eq!(
            owner(dir.path()).as_deref(),
            Some("99999"),
            "the owner's refreshed lock must be put back"
        );
    }

    /// The first pid write into a new lock fails.
    struct PidWriteFails(Cell<bool>);

    impl LockFs for PidWriteFails {
        fn write_pid(&self, file: &mut File, pid: &str) -> io::Result<()> {
            if !self.0.replace(true) {
                return Err(io::Error::other("injected pid write failure"));
            }
            file.write_all(pid.as_bytes())
        }
    }

    #[test]
    fn failed_pid_write_leaves_no_ownerless_lock() {
        let dir = TempDir::new();
        assert_eq!(
            sweeps(dir.path(), None, &PidWriteFails(Cell::new(false))),
            1
        );
        assert_eq!(owner(dir.path()), None);
    }

    #[test]
    fn lock_taken_over_during_a_stall_is_not_released() {
        let dir = TempDir::new();
        serialized(dir.path(), None, &RealFs, |_| {
            fs::write(dir.path().join(".lock"), "77777").unwrap();
        });
        assert_eq!(owner(dir.path()).as_deref(), Some("77777"));
    }

    #[test]
    fn newer_sweep_covers_an_event_but_not_a_reset() {
        let dir = TempDir::new();
        let ahead = SystemTime::now() + Duration::from_secs(60);
        File::create(dir.path().join(".last-sweep"))
            .unwrap()
            .set_modified(ahead)
            .unwrap();
        assert_eq!(sweeps(dir.path(), None, &RealFs), 0);
        assert_eq!(sweeps(dir.path(), Some("w1"), &RealFs), 1);
    }

    #[test]
    fn reset_blocked_by_a_busy_lock_says_so() {
        let dir = TempDir::new();
        foreign_lock(dir.path(), "99999", 0);
        take_stderr();
        assert_eq!(sweeps(dir.path(), Some("w1"), &RealFs), 0);
        assert!(take_stderr().contains("reset of w1 skipped"));
    }
}
