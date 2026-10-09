//! Single-instance enforcement via a PID lockfile with kernel-backed
//! ownership (`flock(2)` on Unix; `LockFileEx` on Windows).
//!
//! Why kernel-backed and not "is the recorded PID alive?":
//! - If the daemon crashes (SIGKILL, segfault, OOM), the file persists
//!   with the dead PID. If the OS later recycles that PID for an unrelated
//!   process, a live-PID probe says "alive" and the new daemon falsely
//!   reports `AlreadyRunning` — while `daemon status` correctly reports
//!   "not running" because nothing owns the socket. The two surfaces
//!   disagree.
//! - `flock(LOCK_EX | LOCK_NB)` / `LockFileEx(LOCKFILE_EXCLUSIVE_LOCK |
//!   LOCKFILE_FAIL_IMMEDIATELY)` tie ownership to the file descriptor.
//!   The kernel releases the lock when the owning process dies, however
//!   it dies. A surviving pidfile with no holder is, by construction,
//!   stale — no PID-recycling false positive is possible.
//!
//! Lifecycle:
//! 1. `acquire(state_dir)` opens `daemon.pid` (creating it if missing)
//!    and attempts a non-blocking exclusive lock.
//! 2. If the lock is contended, another live daemon holds it; read the
//!    pid for a friendly message and return `AlreadyRunning`.
//! 3. If the lock succeeds, we own it. Truncate the file and write our
//!    PID. The held `File` keeps the lock for the daemon lifetime.
//! 4. On `Drop`, close the fd / handle (kernel releases the lock) and
//!    unlink the file. A SIGKILL'd or `TerminateProcess`'d daemon still
//!    releases the lock at handle-teardown time; the file is left behind,
//!    which is exactly the "stale" case the next `acquire` is built to
//!    handle.

use std::{
  fs::{File, OpenOptions},
  io::{self, Read, Seek, SeekFrom, Write},
  path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::{fs::MetadataExt, fs::OpenOptionsExt, io::AsRawFd};

#[cfg(all(unix, target_vendor = "apple"))]
const FILE_TYPE_MASK: u32 = libc::S_IFMT as u32;
#[cfg(all(unix, not(target_vendor = "apple")))]
const FILE_TYPE_MASK: u32 = libc::S_IFMT;

#[cfg(all(unix, target_vendor = "apple"))]
const REGULAR_FILE_MODE: u32 = libc::S_IFREG as u32;
#[cfg(all(unix, not(target_vendor = "apple")))]
const REGULAR_FILE_MODE: u32 = libc::S_IFREG;

/// Result of `acquire`.
#[derive(Debug)]
pub enum AcquireOutcome {
  /// We hold the lock. `path` is the file we created; it's removed when
  /// the returned `Lockfile` is dropped.
  Acquired(Lockfile),
  /// Another live process already owns the lock. The caller should exit
  /// gracefully (typically with code 0 and a "daemon already running"
  /// message).
  AlreadyRunning { pid: i32, path: PathBuf },
}

/// Owned lockfile. Removes the file on drop. Held by the daemon for its
/// entire lifetime so the kernel-backed `flock` survives the whole run.
#[derive(Debug)]
pub struct Lockfile {
  path: PathBuf,
  /// Set by [`Lockfile::disarm`] when the caller has already dealt with the
  /// path itself and `Drop` must not touch whatever lives there now.
  disarmed: bool,
  /// Held open for the daemon lifetime. Closing the fd releases the
  /// `flock` automatically (the kernel does this on process exit too,
  /// which is what gives us recycled-PID safety).
  _file: File,
}

impl Lockfile {
  pub fn path(&self) -> &Path {
    &self.path
  }

  /// Keep the lock, leave the file: `Drop` closes the fd (releasing the
  /// `flock`) but does not unlink. For a caller that removed the path while
  /// the lock was held, where a new owner may already have created its own
  /// file under the same name.
  pub fn disarm(&mut self) {
    self.disarmed = true;
  }
}

impl Drop for Lockfile {
  fn drop(&mut self) {
    if self.disarmed {
      return;
    }
    if let Err(e) = std::fs::remove_file(&self.path) {
      if e.kind() != io::ErrorKind::NotFound {
        log::warn!("failed to remove lockfile {}: {e}", self.path.display());
      }
    }
  }
}

/// Errors that prevent `acquire` from reaching a definitive answer. A
/// healthy daemon never returns these.
#[derive(Debug, thiserror::Error)]
pub enum LockfileError {
  /// State directory was missing or unwritable.
  #[error("could not prepare state dir: {0}")]
  StateDir(#[source] io::Error),
  /// Lockfile content was unreadable or corrupt.
  #[error("lockfile {} is corrupt ({reason}); remove it and retry", path.display())]
  CorruptLockfile { path: PathBuf, reason: String },
  /// Filesystem error not covered by the cases above.
  #[error("lockfile i/o: {0}")]
  Io(#[source] io::Error),
}

// Manual `From<io::Error>` because two variants in this enum wrap
// `io::Error` (`StateDir` and `Io`), so `#[from]` cannot disambiguate.
// The fallback is `Io` — the more specific `StateDir` is opted into by
// the explicit `LockfileError::StateDir(e)` constructor.
impl From<io::Error> for LockfileError {
  fn from(e: io::Error) -> Self {
    LockfileError::Io(e)
  }
}

/// Try to acquire the PID lockfile at `state_dir/daemon.pid`. Creates
/// `state_dir` if it doesn't exist. See module docs for the policy.
pub fn acquire(state_dir: &Path) -> Result<AcquireOutcome, LockfileError> {
  std::fs::create_dir_all(state_dir).map_err(LockfileError::StateDir)?;
  let path = state_dir.join("daemon.pid");

  let mut opts = OpenOptions::new();
  opts.read(true).write(true).create(true).truncate(false);
  #[cfg(unix)]
  {
    opts.mode(0o600);
    // `O_NOFOLLOW` refuses to open a symlink (returns ELOOP). Without
    // this, a local attacker on macOS could plant
    // `/tmp/llamastash-$USER/daemon.pid → /victim/critical/file` and the
    // subsequent `set_len(0)` would truncate the victim file. Linux's
    // XDG_RUNTIME_DIR is 0700 so the attack is macOS-fallback-only, but
    // the flag is cheap and we want defence in depth.
    opts.custom_flags(libc::O_NOFOLLOW);
  }
  let mut file = opts.open(&path)?;
  // Belt-and-braces: refuse to operate on anything other than a regular
  // file. `O_NOFOLLOW` already rejects symlinks; this catches the
  // pre-existing-FIFO / pre-existing-device-node shape.
  #[cfg(unix)]
  {
    let meta = file.metadata()?;
    // `MetadataExt::mode()` is `u32`, while Apple's libc exposes these
    // file-type constants as `u16`.
    let mode = meta.mode() & FILE_TYPE_MASK;
    if mode != REGULAR_FILE_MODE {
      return Err(LockfileError::CorruptLockfile {
        path: path.clone(),
        reason: format!("not a regular file (mode {mode:o})"),
      });
    }
  }

  match try_flock_exclusive(&file)? {
    FlockOutcome::Acquired => {
      // We own the lock — overwrite the file with our PID.
      file.seek(SeekFrom::Start(0))?;
      file.set_len(0)?;
      writeln!(file, "{}", std::process::id())?;
      file.sync_all()?;
      Ok(AcquireOutcome::Acquired(Lockfile {
        path,
        disarmed: false,
        _file: file,
      }))
    }
    FlockOutcome::Contended => {
      // Another process holds the lock — read its PID for a friendly
      // message. The PID is informational only; ownership is decided by
      // the flock itself, so even if the file is mid-write we still
      // correctly report contention.
      let pid = read_pid(&path).unwrap_or(0);
      Ok(AcquireOutcome::AlreadyRunning { pid, path })
    }
  }
}

#[derive(Debug)]
enum FlockOutcome {
  Acquired,
  Contended,
}

/// Attempt a non-blocking exclusive `flock` on `file`. Returns
/// `Contended` if another process holds the lock, `Acquired` if we now
/// hold it. Any other error is propagated.
#[cfg(unix)]
fn try_flock_exclusive(file: &File) -> io::Result<FlockOutcome> {
  // SAFETY: `flock(2)` is a kernel syscall that operates on a borrowed
  // file descriptor; no memory is touched. The fd outlives the call
  // because `file` is a borrowed reference.
  let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
  if ret == 0 {
    return Ok(FlockOutcome::Acquired);
  }
  let err = io::Error::last_os_error();
  match err.raw_os_error() {
    // EWOULDBLOCK on Linux and macOS both signal "already locked by
    // another process". POSIX permits returning either EWOULDBLOCK or
    // EAGAIN; libc aliases EWOULDBLOCK to EAGAIN on every platform we
    // target.
    Some(code) if code == libc::EWOULDBLOCK => Ok(FlockOutcome::Contended),
    _ => Err(err),
  }
}

/// Windows backend: `LockFileEx` with `LOCKFILE_EXCLUSIVE_LOCK |
/// LOCKFILE_FAIL_IMMEDIATELY` covers the same kernel-backed-ownership
/// contract as POSIX `flock`. The kernel releases the lock when the
/// HANDLE closes — including via `TerminateProcess` — so a stale
/// pidfile left behind by an ungraceful daemon death is re-acquirable.
///
/// Unlike Unix `flock` (which is whole-file advisory), Windows
/// `LockFileEx` is byte-range and mandatory: any handle that overlaps
/// the locked range fails reads with `ERROR_LOCK_VIOLATION`. We lock
/// a single byte at a sentinel offset (`SENTINEL_OFFSET`) well past
/// the few bytes of ASCII PID at offset 0 so the daemon-status surface
/// and the contended-acquire path can both `fs::read_to_string` the
/// pidfile without tripping the lock.
#[cfg(windows)]
fn try_flock_exclusive(file: &File) -> io::Result<FlockOutcome> {
  use std::os::windows::io::AsRawHandle;
  use windows_sys::Win32::Storage::FileSystem::{
    LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
  };
  use windows_sys::Win32::System::IO::OVERLAPPED;

  // SAFETY: `OVERLAPPED` is a plain-old-data struct; zero-init matches
  // the documented "synchronous-only" use (no events).
  let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
  // Place the 1-byte lock at offset 2^32-1 (~4 GiB). The pidfile is
  // ~10 bytes; readers touching the actual content range are not
  // affected. `OVERLAPPED.Anonymous.Anonymous` is a union access —
  // safe because we only ever write to the struct variant (never the
  // Pointer variant) on this code path.
  overlapped.Anonymous.Anonymous.Offset = u32::MAX;
  overlapped.Anonymous.Anonymous.OffsetHigh = 0;

  // SAFETY: handle is borrowed from `file` (lives as long as the call);
  // `&mut overlapped` is a writable OVERLAPPED the kernel reads/writes.
  let ok = unsafe {
    LockFileEx(
      file.as_raw_handle() as _,
      LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
      0,
      1, // nNumberOfBytesToLockLow — lock a single sentinel byte
      0, // nNumberOfBytesToLockHigh
      &mut overlapped as *mut _,
    )
  };
  if ok != 0 {
    return Ok(FlockOutcome::Acquired);
  }
  let err = io::Error::last_os_error();
  // ERROR_LOCK_VIOLATION (33) is what LockFileEx returns when another
  // process already holds the requested range. ERROR_IO_PENDING (997)
  // shouldn't fire with LOCKFILE_FAIL_IMMEDIATELY but defensively map
  // it to Contended too.
  const ERROR_LOCK_VIOLATION: i32 = 33;
  const ERROR_IO_PENDING: i32 = 997;
  match err.raw_os_error() {
    Some(ERROR_LOCK_VIOLATION) | Some(ERROR_IO_PENDING) => Ok(FlockOutcome::Contended),
    _ => Err(err),
  }
}

fn read_pid(path: &Path) -> Result<i32, LockfileError> {
  let mut contents = String::new();
  File::open(path)?.read_to_string(&mut contents)?;
  let trimmed = contents.trim();
  if trimmed.is_empty() {
    return Ok(0);
  }
  trimmed
    .parse::<i32>()
    .map_err(|e| LockfileError::CorruptLockfile {
      path: path.to_path_buf(),
      reason: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn temp_state_dir(name: &str) -> PathBuf {
    crate::util::test_temp::unique_temp_dir(&format!("lockfile-{name}"))
  }

  #[test]
  fn acquire_creates_pidfile_when_absent() {
    let dir = temp_state_dir("fresh");
    let outcome = acquire(&dir).expect("acquire should succeed");
    match outcome {
      AcquireOutcome::Acquired(lock) => {
        let raw = std::fs::read_to_string(lock.path()).expect("pidfile readable");
        assert_eq!(raw.trim(), std::process::id().to_string());
      }
      AcquireOutcome::AlreadyRunning { pid, .. } => panic!("unexpected AlreadyRunning(pid={pid})"),
    }
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn disarm_leaves_the_path_alone_and_releases_the_lock() {
    let dir = temp_state_dir("disarm");
    let path = {
      let mut lock = match acquire(&dir).expect("acquire") {
        AcquireOutcome::Acquired(l) => l,
        AcquireOutcome::AlreadyRunning { .. } => panic!("unexpected AlreadyRunning"),
      };
      let p = lock.path().to_path_buf();
      std::fs::remove_file(&p).expect("unlink while held");
      // Somebody recreated the name after we unlinked ours; disarming means
      // `Drop` must not treat that file as ours.
      std::fs::write(&p, b"9999\n").unwrap();
      lock.disarm();
      drop(lock);
      p
    };
    assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "9999");
    // And the lock really went with the drop, not with the process.
    match acquire(&dir).expect("re-acquire after disarm") {
      AcquireOutcome::Acquired(_) => {}
      AcquireOutcome::AlreadyRunning { pid, .. } => panic!("still held by {pid}"),
    }
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn drop_removes_pidfile() {
    let dir = temp_state_dir("drop");
    let path = {
      let lock = match acquire(&dir).expect("acquire") {
        AcquireOutcome::Acquired(l) => l,
        AcquireOutcome::AlreadyRunning { .. } => panic!("unexpected AlreadyRunning"),
      };
      let p = lock.path().to_path_buf();
      drop(lock);
      p
    };
    assert!(
      !path.exists(),
      "drop must remove the pidfile, still at {}",
      path.display()
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn second_acquire_with_live_lock_reports_already_running() {
    let dir = temp_state_dir("live");
    let _first = match acquire(&dir).expect("first acquire") {
      AcquireOutcome::Acquired(l) => l,
      AcquireOutcome::AlreadyRunning { .. } => panic!("unexpected AlreadyRunning"),
    };
    let outcome = acquire(&dir).expect("second acquire");
    match outcome {
      AcquireOutcome::AlreadyRunning { pid, .. } => {
        assert_eq!(pid, std::process::id() as i32);
      }
      AcquireOutcome::Acquired(_) => panic!("second acquire should observe live flock"),
    }
    drop(_first);
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn stale_pidfile_without_flock_is_re_acquired() {
    // Pre-seed a pidfile but do NOT hold an flock on it. This is the
    // "crashed daemon" shape: the file persists but the kernel released
    // its lock when the daemon's fd closed. acquire must succeed even
    // though the recorded pid (here: our own) is provably alive.
    let dir = temp_state_dir("stale-no-flock");
    let path = dir.join("daemon.pid");
    let live_but_unrelated_pid = std::process::id() as i32;
    std::fs::write(&path, format!("{live_but_unrelated_pid}\n")).expect("seed pidfile");

    let outcome = acquire(&dir).expect("acquire over stale pidfile");
    match outcome {
      AcquireOutcome::Acquired(lock) => {
        let raw = std::fs::read_to_string(lock.path()).expect("readable");
        assert_eq!(
          raw.trim(),
          std::process::id().to_string(),
          "pidfile must be rewritten to our pid"
        );
      }
      AcquireOutcome::AlreadyRunning { pid, .. } => {
        panic!(
          "stale pidfile with no flock holder should be acquirable, got AlreadyRunning(pid={pid})"
        )
      }
    }
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn arbitrary_dead_pid_is_re_acquired() {
    let dir = temp_state_dir("stale-dead-pid");
    let path = dir.join("daemon.pid");
    // 2^31 - 1 is the kernel pid_max ceiling on 64-bit Linux and won't
    // be allocated under normal conditions — but with flock-based
    // ownership the PID value is irrelevant; what matters is no live
    // process holds the lock.
    std::fs::write(&path, "2147483646\n").expect("seed stale pidfile");
    let outcome = acquire(&dir).expect("acquire");
    match outcome {
      AcquireOutcome::Acquired(lock) => {
        let raw = std::fs::read_to_string(lock.path()).expect("readable");
        assert_eq!(raw.trim(), std::process::id().to_string());
      }
      AcquireOutcome::AlreadyRunning { pid, .. } => {
        panic!("stale pid {pid} should have been re-acquired")
      }
    }
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn corrupt_pidfile_is_recovered_when_lock_is_free() {
    // With flock-based ownership, corrupt pidfile content is no longer
    // fatal — what matters is the lock state. The file gets rewritten
    // with our PID on successful acquire.
    let dir = temp_state_dir("corrupt");
    let path = dir.join("daemon.pid");
    std::fs::write(&path, "this is not a pid").expect("seed corrupt pidfile");
    let outcome = acquire(&dir).expect("acquire over corrupt pidfile");
    match outcome {
      AcquireOutcome::Acquired(lock) => {
        let raw = std::fs::read_to_string(lock.path()).expect("readable");
        assert_eq!(raw.trim(), std::process::id().to_string());
      }
      AcquireOutcome::AlreadyRunning { pid, .. } => {
        panic!("corrupt pidfile with no flock holder should be acquirable, got pid={pid}")
      }
    }
    std::fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn pidfile_mode_is_0600_on_unix() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_state_dir("perms");
    let lock = match acquire(&dir).expect("acquire") {
      AcquireOutcome::Acquired(l) => l,
      AcquireOutcome::AlreadyRunning { .. } => panic!("unexpected AlreadyRunning"),
    };
    let mode = std::fs::metadata(lock.path())
      .expect("metadata")
      .permissions()
      .mode()
      & 0o777;
    assert_eq!(mode, 0o600, "pidfile must be 0600");
    drop(lock);
    std::fs::remove_dir_all(&dir).ok();
  }

  #[cfg(unix)]
  #[test]
  fn symlinked_pidfile_is_refused() {
    use std::os::unix::fs::symlink;

    let dir = temp_state_dir("symlink");
    // Pre-create a target file the attacker wants to truncate.
    let victim = dir.join("victim.dat");
    std::fs::write(&victim, b"important data").expect("write victim");
    let pidfile = dir.join("daemon.pid");
    symlink(&victim, &pidfile).expect("plant symlink");

    let err = acquire(&dir).expect_err("symlink at daemon.pid must be refused");
    // We get back an io error (ELOOP from O_NOFOLLOW). Body is allowed
    // to vary across libc versions; the important thing is that we
    // didn't open the symlink target.
    assert!(matches!(err, LockfileError::Io(_)));
    // Victim must be untouched.
    let after = std::fs::read(&victim).expect("victim still readable");
    assert_eq!(after, b"important data");
    std::fs::remove_dir_all(&dir).ok();
  }
}
