//! Cross-platform secret-file hardening.
//!
//! `runtime.json` and `state.json` carry per-daemon secrets (the bearer
//! token, model paths, last-launch params) and must not be readable by
//! other accounts on the machine. On Unix this is the explicit
//! `chmod 0o600` applied during atomic-write. On Windows there's no
//! mode-bit equivalent; we apply a Protected DACL that grants
//! Generic-All to the file owner and no inheritance from the parent.
//!
//! Unix mode-bit application lives in [`crate::util::atomic_write`].
//! This module only contains the Windows surface; on Unix the
//! `set_owner_only_dacl` function is a no-op compile-time stub.
//!
//! The SDDL string `D:P(A;;GA;;;OW)` expands as:
//! - `D:` — DACL section
//! - `P` — Protected (no inheritance from the parent)
//! - `(A;;GA;;;OW)` — Allow ACE granting Generic All to the OWner
//!
//! Best-effort by design: a failure to apply the DACL is logged and
//! swallowed. The state directory is already under `%LOCALAPPDATA%`,
//! which inherits a per-user ACL, so the file is non-readable to
//! other users even without explicit hardening. The DACL apply here
//! is belt-and-suspenders against misconfigured parent ACLs.
//!
//! On Unix the module also hosts the shared directory swap-surface
//! check ([`dir_swap_surface`]) used by the init binary-adoption,
//! config-write, and doctor preflights. A directory is a "swap
//! surface" when a user other than its owner could rename/replace a
//! file inside it, which would let an attacker substitute the adopted
//! `llama-server` or a `0600` config.

use std::path::Path;

/// Apply an owner-only DACL to `path` on Windows (no-op on Unix —
/// caller's `chmod` already hardened the file). Best-effort: failures
/// are swallowed with a warning. Safe to call repeatedly.
#[cfg(windows)]
pub fn set_owner_only_dacl(path: &Path) {
  use std::os::windows::ffi::OsStrExt;
  use windows_sys::Win32::Foundation::LocalFree;
  use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
  };
  use windows_sys::Win32::Security::{SetFileSecurityW, DACL_SECURITY_INFORMATION};

  // Owner = Generic All, Protected DACL (no inheritance from parent).
  // OW = owner of the object (current user creating the file).
  let sddl: Vec<u16> = "D:P(A;;GA;;;OW)\0".encode_utf16().collect();
  let mut sd_ptr: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR = std::ptr::null_mut();
  // SAFETY: SDDL is a NUL-terminated UTF-16 string; sd_ptr receives a
  // freshly-allocated security descriptor we LocalFree below.
  let built = unsafe {
    ConvertStringSecurityDescriptorToSecurityDescriptorW(
      sddl.as_ptr(),
      SDDL_REVISION_1,
      &mut sd_ptr,
      std::ptr::null_mut(),
    )
  };
  if built == 0 {
    log::warn!(
      "could not build security descriptor for {}: {}",
      path.display(),
      std::io::Error::last_os_error()
    );
    return;
  }
  let path_w: Vec<u16> = path
    .as_os_str()
    .encode_wide()
    .chain(std::iter::once(0))
    .collect();
  // SAFETY: sd_ptr is a valid SD from the call above; path_w is
  // NUL-terminated UTF-16; SetFileSecurityW reads both and returns
  // BOOL.
  let applied = unsafe { SetFileSecurityW(path_w.as_ptr(), DACL_SECURITY_INFORMATION, sd_ptr) };
  if applied == 0 {
    log::warn!(
      "could not apply owner-only DACL to {}: {}",
      path.display(),
      std::io::Error::last_os_error()
    );
  }
  // SAFETY: sd_ptr was allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW
  // per its documented contract: caller must LocalFree.
  unsafe {
    LocalFree(sd_ptr as _);
  }
}

#[cfg(not(windows))]
pub fn set_owner_only_dacl(_path: &Path) {
  // Unix files are hardened via `chmod 0o600` in atomic_write::write_secure.
}

/// Why `path` is a swap surface, or nothing when it is safe to hold
/// trusted files (an adopted binary, a `0600` config).
///
/// The rule is **owner-aware**: what matters is whether a user *other
/// than the directory's owner* can write into it, not the raw mode bits.
/// - A directory owned by the caller is safe against other users even
///   when group-writable — the group is the owner's own choice, and for
///   a user-private group (the home-dir default) it contains no one
///   else. Refusing it would block legitimate installs like a `mise`
///   manage tree under `~/.local/share`.
/// - A world-writable directory admits any account, owned or not.
/// - A group-writable directory owned by root or another account admits
///   that group's members, who are not the owner.
/// - Any directory owned by a non-root account other than the caller is
///   controlled by that account regardless of mode bits (the owner can
///   always `chmod`/rename inside it), so it is a swap surface too.
///
/// Missing/metadata-failing paths report `None`, matching the historical
/// skip-not-fail behavior: a path that can't be statted isn't a
/// directory we can adopt into anyway.
#[cfg(unix)]
pub fn dir_swap_surface(path: &Path, our_uid: u32) -> Option<SwapSurface> {
  use std::os::unix::fs::{MetadataExt, PermissionsExt};
  let meta = std::fs::metadata(path).ok()?;
  let owner = meta.uid();
  let mode = meta.permissions().mode() & 0o777;
  SwapSurface::for_perm(owner, mode, our_uid)
}

/// Classification of a directory that lets a non-owner write into it.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapSurface {
  /// Owned by another non-root account: that account controls it
  /// regardless of mode bits.
  ForeignOwner { owner: u32, mode: u32 },
  /// World-write bit set: any account on the machine can write.
  WorldWritable { mode: u32 },
  /// Group-write bit set on a directory whose owner isn't the caller:
  /// members of the owning group (root's or another account's) can write.
  ForeignOwnerGroupWritable { owner: u32, mode: u32 },
}

#[cfg(unix)]
impl SwapSurface {
  fn for_perm(owner: u32, mode: u32, our_uid: u32) -> Option<SwapSurface> {
    if owner != our_uid && owner != 0 {
      return Some(SwapSurface::ForeignOwner { owner, mode });
    }
    let g = mode & 0o777;
    if g & 0o002 != 0 {
      return Some(SwapSurface::WorldWritable { mode });
    }
    // group-write on a dir we own is the owner's own group choice; on a
    // root-owned dir it hands write access to a group of other accounts.
    if g & 0o020 != 0 && owner != our_uid {
      return Some(SwapSurface::ForeignOwnerGroupWritable { owner, mode });
    }
    None
  }

  /// One-line reason for the calling preflight's error/warning message.
  pub fn describe(self, our_uid: u32) -> String {
    match self {
      SwapSurface::ForeignOwner { owner, .. } => format!(
        "is owned by UID {owner} (neither you, UID {our_uid}, nor root); that account controls it"
      ),
      SwapSurface::WorldWritable { mode } => {
        format!("is world-writable (mode {mode:#o}); any user could replace a file inside it")
      }
      SwapSurface::ForeignOwnerGroupWritable { owner, mode } => format!(
        "is group-writable (mode {mode:#o}) and owned by another account (UID {owner}); \
         members of its group could replace a file inside it"
      ),
    }
  }
}

#[cfg(all(test, unix))]
mod tests_unix {
  use super::*;
  use std::fs;
  use std::os::unix::fs::PermissionsExt;

  #[test]
  fn for_perm_accepts_self_owned_group_writable_dir() {
    // The reported false positive: a group-writable directory you own
    // (the home-dir / `mise` installs case) has no other user who can
    // write, so it is not a swap surface.
    assert_eq!(
      SwapSurface::for_perm(1000, 0o775, 1000),
      None,
      "self-owned group-writable dir must be safe"
    );
    assert_eq!(
      SwapSurface::for_perm(1000, 0o755, 1000),
      None,
      "plain self-owned dir must be safe"
    );
  }

  #[test]
  fn for_perm_refuses_world_writable_regardless_of_owner() {
    assert_eq!(
      SwapSurface::for_perm(1000, 0o777, 1000),
      Some(SwapSurface::WorldWritable { mode: 0o777 })
    );
    // Even root-owned, world-write admits any account.
    assert_eq!(
      SwapSurface::for_perm(0, 0o777, 1000),
      Some(SwapSurface::WorldWritable { mode: 0o777 })
    );
  }

  #[test]
  fn for_perm_refuses_group_writable_foreign_owned_dir() {
    // A root-owned, group-writable dir hands write to a group of other
    // accounts; a dir owned by another non-root user is theirs outright.
    assert_eq!(
      SwapSurface::for_perm(0, 0o775, 1000),
      Some(SwapSurface::ForeignOwnerGroupWritable {
        owner: 0,
        mode: 0o775
      })
    );
    assert_eq!(
      SwapSurface::for_perm(999, 0o755, 1000),
      Some(SwapSurface::ForeignOwner {
        owner: 999,
        mode: 0o755
      }),
      "a dir owned by another account is controlled by them even at 0755"
    );
  }

  #[test]
  fn real_dir_matches_the_predicate() {
    let dir = crate::util::test_temp::unique_temp_dir("swap-surface");
    let our_uid = unsafe { libc::geteuid() };

    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(dir_swap_surface(&dir, our_uid), None);

    fs::set_permissions(&dir, fs::Permissions::from_mode(0o775)).unwrap();
    assert_eq!(
      dir_swap_surface(&dir, our_uid),
      None,
      "self-owned 0775 is safe"
    );

    fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
      dir_swap_surface(&dir, our_uid),
      Some(SwapSurface::WorldWritable { mode: 0o777 })
    );

    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
      dir_swap_surface(&dir.join("does-not-exist"), our_uid),
      None,
      "an unstat-able path skips the check"
    );
    fs::remove_dir_all(&dir).ok();
  }
}

#[cfg(all(test, windows))]
mod tests_windows {
  use super::*;
  use std::time::{SystemTime, UNIX_EPOCH};

  fn temp_path(label: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("clock")
      .as_nanos();
    let p = std::env::temp_dir().join(format!(
      "llamastash-dacl-{label}-{}-{nanos}.tmp",
      std::process::id()
    ));
    std::fs::write(&p, b"secret").expect("seed file");
    p
  }

  #[test]
  fn set_owner_only_dacl_no_panic_on_existing_file() {
    // We don't try to verify the resulting DACL programmatically here —
    // that requires more Win32 plumbing than the test deserves. The
    // contract is "best-effort, no panic, no harm to the file";
    // verifying the call returns without panicking and that the file
    // is still readable is enough.
    let p = temp_path("apply");
    set_owner_only_dacl(&p);
    let read = std::fs::read(&p).expect("file still readable by owner");
    assert_eq!(read, b"secret");
    std::fs::remove_file(&p).ok();
  }

  #[test]
  fn set_owner_only_dacl_no_panic_on_missing_file() {
    // Best-effort contract: missing file just logs a warning.
    let p = temp_path("missing");
    std::fs::remove_file(&p).expect("remove");
    set_owner_only_dacl(&p);
  }
}
