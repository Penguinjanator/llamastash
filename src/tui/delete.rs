//! Resolve every file a "delete this model" must remove, then remove it.
//!
//! A catalog row is rarely one file on disk. A split set carries shards
//! 2..N, a multimodal model carries an `mmproj-*.gguf` projector, an
//! MTP-capable model can carry a separate `mtp-*.gguf` draft head, and a
//! HuggingFace snapshot entry is a symlink whose bytes live in a sibling
//! `blobs/` file. Unlinking just the launch path leaves the rest behind and
//! the freed space never shows up.
//!
//! The plan is resolved before the confirm prompt so the popup can say what
//! is about to go and the confirm handler does no fresh discovery.

use std::fs;
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::discovery::{scanner, DiscoveredModel};

/// Files a confirmed delete will remove.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeletePlan {
  /// The row's own file — shard 1 of a split set, else the model itself.
  pub primary: PathBuf,
  /// Shards 2..N of a split set. Empty for a single-file model.
  pub shards: Vec<PathBuf>,
  /// The mmproj projector companion, when no other model in the folder
  /// pairs with it.
  pub projector: Option<PathBuf>,
  /// The separate MTP draft head, under the same exclusivity rule.
  pub mtp_head: Option<PathBuf>,
  /// Other precisions of the companion taken above (`mmproj-BF16.gguf` beside the
  /// paired `mmproj-F16.gguf`). Their names carry a dtype and no model, so nothing
  /// pairs with them once this model is gone.
  pub orphan_companions: Vec<PathBuf>,
  /// HuggingFace cache repo directory to remove wholesale, blobs and all.
  /// Set only when this row is the last model in that repo — with a second
  /// quant still there the per-file path runs instead so the survivor keeps
  /// its bytes.
  pub hf_repo_dir: Option<PathBuf>,
  /// Resolved HF cache root, carried so [`execute`] can decide whether a
  /// snapshot symlink's blob is ours to unlink.
  cache_root: Option<PathBuf>,
  /// Whether the files sit in the HF cache, so [`DeletePlan::describe`] only
  /// promises reclaimed blobs where there are blobs to reclaim.
  in_hf_cache: bool,
}

impl DeletePlan {
  /// A plan that unlinks one file and nothing else.
  pub fn single(primary: impl Into<PathBuf>) -> Self {
    DeletePlan {
      primary: primary.into(),
      ..DeletePlan::default()
    }
  }

  /// Every file the plan unlinks individually, primary first. Empty when
  /// the plan removes a whole repo directory instead.
  pub fn files(&self) -> Vec<&Path> {
    let mut out = vec![self.primary.as_path()];
    out.extend(self.shards.iter().map(PathBuf::as_path));
    out.extend(self.projector.as_deref());
    out.extend(self.mtp_head.as_deref());
    out.extend(self.orphan_companions.iter().map(PathBuf::as_path));
    out
  }

  /// Confirm-popup body: what is going, and why it is more than one file.
  /// One flowing paragraph — the overlay renders a single wrapped `Line`, so
  /// embedded newlines would collapse mid-sentence rather than break.
  pub fn describe(&self, display_name: &str) -> String {
    let head = format!("Delete `{display_name}` from disk?");
    if let Some(repo) = &self.hf_repo_dir {
      let repo_name = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo.display().to_string());
      return format!(
        "{head} It is the last model in the HuggingFace cache repo \
         `{repo_name}`, so the whole directory goes — every revision, shard \
         and blob."
      );
    }
    let mut extras: Vec<String> = Vec::new();
    if !self.shards.is_empty() {
      extras.push(format!(
        "{} more shard{}",
        self.shards.len(),
        if self.shards.len() == 1 { "" } else { "s" }
      ));
    }
    if self.projector.is_some() {
      extras.push("its mmproj projector".to_string());
    }
    if self.mtp_head.is_some() {
      extras.push("its MTP draft head".to_string());
    }
    if !self.orphan_companions.is_empty() {
      extras.push(format!(
        "{} unpaired companion precision{}",
        self.orphan_companions.len(),
        if self.orphan_companions.len() == 1 {
          ""
        } else {
          "s"
        }
      ));
    }
    let blobs = if self.in_hf_cache {
      " The HuggingFace cache blobs behind them are freed too."
    } else {
      ""
    };
    match extras.len() {
      // A whole-directory row removes a tree, not a file. Saying "one file"
      // there understates an irreversible action on the one shape where the
      // user is least able to guess what it covers.
      0 if self.primary.is_dir() => {
        format!("{head} The whole model directory goes — every file inside it.{blobs}")
      }
      0 => format!("{head} One file is unlinked.{blobs}"),
      _ => format!(
        "{head} {} files go: the model plus {}.{blobs}",
        self.files().len(),
        join_human(&extras)
      ),
    }
  }
}

/// Build the delete plan for `target`.
///
/// `catalog` is the live model list, used for two exclusivity questions a
/// lone path cannot answer: does another model in the same folder also pair
/// with this companion, and is another model still living in this HF repo.
pub fn plan(
  target: &DiscoveredModel,
  catalog: &[DiscoveredModel],
  cache_root: Option<&Path>,
) -> DeletePlan {
  let mut plan = DeletePlan {
    primary: target.path.clone(),
    shards: target.split_siblings.clone(),
    cache_root: cache_root.map(Path::to_path_buf),
    ..DeletePlan::default()
  };

  let neighbours: Vec<&DiscoveredModel> = catalog
    .iter()
    .filter(|m| m.path != target.path && m.parent == target.parent)
    .collect();

  // Companions are a GGUF-file notion. A whole-directory row (a safetensors
  // snapshot) owns none, and the finders would walk the enclosing `snapshots/`
  // dir for an mmproj that cannot be there.
  if !target.path.is_dir() {
    // Resolved from disk rather than from the catalog's capability flags: a
    // projector whose own header won't parse leaves `multimodal` unset but is
    // still a file this model owns. Both finders are a directory walk, and the
    // per-neighbour re-resolution below only runs once one actually matched.
    plan.projector = scanner::find_mmproj(&target.path).filter(|proj| {
      !neighbours
        .iter()
        .any(|m| scanner::find_mmproj(&m.path).as_deref() == Some(proj.as_path()))
    });
    let arch = target.metadata.as_ref().and_then(|m| m.arch.as_deref());
    plan.mtp_head = scanner::find_mtp_head(&target.path, arch).filter(|head| {
      !neighbours.iter().any(|m| {
        let neighbour_arch = m.metadata.as_ref().and_then(|md| md.arch.as_deref());
        scanner::find_mtp_head(&m.path, neighbour_arch).as_deref() == Some(head.as_path())
      })
    });
    // A companion folder can hold the same file at several precisions. Only the
    // best-ranked one pairs, so the rest are orphans here and nowhere else — they
    // go with this model rather than sitting next to nothing.
    plan.orphan_companions = plan
      .projector
      .as_deref()
      .map(scanner::companion_precisions)
      .unwrap_or_default()
      .into_iter()
      .chain(
        plan
          .mtp_head
          .as_deref()
          .map(scanner::companion_precisions)
          .unwrap_or_default(),
      )
      .collect();
  }

  // Whole-repo removal reclaims the non-GGUF cache cruft (refs, configs,
  // stale revisions) that per-file unlinking cannot see, but only once
  // nothing else in the repo is still listed.
  if let Some(repo_dir) = hf_repo_dir_in_cache(&target.path, cache_root) {
    plan.in_hf_cache = true;
    let shape = hf_repo_dir_shape(&target.path);
    let repo_still_populated = catalog
      .iter()
      .any(|m| m.path != target.path && hf_repo_dir_shape(&m.path) == shape);
    if !repo_still_populated {
      plan.hf_repo_dir = Some(repo_dir);
    }
  }

  plan
}

/// [`plan`] against the live HF cache root, resolving `path` to its catalog
/// row. A path with no row (the catalog refreshed underneath the cursor)
/// degrades to a single-file plan rather than refusing.
pub fn plan_for_path(path: &Path, catalog: &[DiscoveredModel]) -> DeletePlan {
  let cache_root = crate::init::download::hf_cache_dir().ok();
  match catalog.iter().find(|m| m.path == path) {
    Some(target) => plan(target, catalog, cache_root.as_deref()),
    None => DeletePlan {
      primary: path.to_path_buf(),
      cache_root,
      ..DeletePlan::default()
    },
  }
}

/// Carry out `plan`, returning a human-readable summary for the toast.
///
/// A missing file is not an error: discovery can lag a delete that happened
/// outside llamastash, and failing the whole operation because a companion
/// was already gone would leave the rest on disk.
pub fn execute(plan: &DeletePlan) -> io::Result<String> {
  // One lock budget for the whole delete, shared by every payload it sweeps.
  let locks_close = Instant::now() + SHARED_BLOB_LOCK_WAIT;
  if let Some(repo_dir) = &plan.hf_repo_dir {
    // `remove_dir_all` unlinks the repo's shared-store symlinks without
    // following them, so collect their targets first and sweep them after.
    let store_blobs: Vec<PathBuf> = fs::read_dir(repo_dir.join("blobs"))
      .into_iter()
      .flatten()
      .flatten()
      .filter_map(|e| shared_store_target(&e.path(), plan.cache_root.as_deref()))
      .collect();
    fs::remove_dir_all(repo_dir)?;
    if let Some(cache_root) = plan.cache_root.as_deref() {
      for store in &store_blobs {
        sweep_shared_blob(store, cache_root, locks_close);
      }
    }
    return Ok(format!(
      "deleted HF cache for {}",
      repo_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("model")
    ));
  }

  // A directory primary cannot go through `remove_file` (EISDIR, surfaced as
  // an opaque failure). Two ways to get here, and they are not the same
  // situation — saying the wrong one sent users looking for a sibling model
  // that does not exist.
  if plan.primary.is_dir() {
    if plan.in_hf_cache {
      return Err(io::Error::other(format!(
        "{} is not the only model in its cache repo — refusing to delete it",
        plan.primary.display()
      )));
    }
    // Outside the resolved cache root: nothing else shares this directory, so
    // removing it is exactly what the user asked for.
    fs::remove_dir_all(&plan.primary)?;
    return Ok(format!(
      "deleted {}",
      plan
        .primary
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("model")
    ));
  }

  let files = plan.files();
  let mut removed = 0usize;
  let mut first_error = None;
  for file in &files {
    match remove_file_and_blob(file, plan.cache_root.as_deref(), locks_close) {
      Ok(()) => removed += 1,
      Err(e) if e.kind() == io::ErrorKind::NotFound => {}
      Err(e) => {
        first_error.get_or_insert(e);
      }
    }
  }
  // Only the primary failing is fatal — a companion we could not unlink is
  // reported but the model itself is gone, which is what the user asked for.
  if !plan.primary.exists() {
    let name = plan
      .primary
      .file_name()
      .and_then(|n| n.to_str())
      .unwrap_or("model");
    return Ok(match removed {
      0 | 1 => format!("deleted {name}"),
      n => format!("deleted {name} + {} companion files", n - 1),
    });
  }
  Err(first_error.unwrap_or_else(|| io::Error::other("delete removed nothing")))
}

/// Unlink `file`, plus the HF cache blob it points at when it is a snapshot
/// symlink into its own repo's `blobs/` directory. A link pointing anywhere
/// else is unlinked without touching its target.
fn remove_file_and_blob(
  file: &Path,
  cache_root: Option<&Path>,
  locks_close: Instant,
) -> io::Result<()> {
  let blob = hf_blob_target(file, cache_root);
  fs::remove_file(file)?;
  if let Some(blob) = blob {
    let store = shared_store_target(&blob, cache_root);
    let _ = fs::remove_file(&blob);
    if let (Some(store), Some(cache_root)) = (store, cache_root) {
      sweep_shared_blob(&store, cache_root, locks_close);
    }
  }
  Ok(())
}

/// The repo's `blobs/<sha>` entry behind an HF snapshot symlink, or `None` when
/// `file` is a plain file, sits outside the cache, or links out of the repo.
/// Resolves one hop only: under huggingface_hub's shared store the entry is
/// itself a symlink into `<cache>/blobs/<xx>/`, and following it there would
/// put the target outside the repo.
fn hf_blob_target(file: &Path, cache_root: Option<&Path>) -> Option<PathBuf> {
  let repo_dir = hf_repo_dir_in_cache(file, cache_root)?;
  let Ok(Some(entry)) = link_target_one_hop(file) else {
    return None;
  };
  (entry.parent()? == repo_dir.join("blobs")).then_some(entry)
}

/// Where the symlink `link` points, with the directory part canonicalised and
/// the last component left as is, so a target that is itself a link stays put.
/// `Ok(None)` when `link` is not a symlink or is gone; `Err` for any other
/// filesystem fault, so a caller about to free bytes can tell a reference that
/// is gone from one it simply could not read.
fn link_target_one_hop(link: &Path) -> io::Result<Option<PathBuf>> {
  let meta = match fs::symlink_metadata(link) {
    Ok(meta) => meta,
    Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
    Err(e) => return Err(e),
  };
  if !meta.file_type().is_symlink() {
    return Ok(None);
  }
  let raw = fs::read_link(link)?;
  let joined = if raw.is_absolute() {
    raw
  } else {
    link.parent().unwrap_or(link).join(raw)
  };
  let (dir, name) = match (joined.parent(), joined.file_name()) {
    (Some(dir), Some(name)) => (dir, name),
    _ => return Ok(None),
  };
  Ok(Some(dir.canonicalize()?.join(name)))
}

/// Marker huggingface_hub writes in `<cache>/blobs/` when it keeps one shared
/// copy per file (`<cache>/blobs/<xx>/<xet_hash>`, since huggingface_hub 1.32.0)
/// and makes each repo's `blobs/<sha>` a symlink to it.
const SHARED_BLOBS_MARKER: &str = ".huggingface-shared-blobs";

/// How long one delete may wait on shared-blob locks in total, across every
/// payload it sweeps. A sweep runs on whichever thread asked for the delete,
/// which for the TUI is its draw thread, so the budget is per delete rather than
/// per payload: a whole-repo delete must not pay it once per blob.
///
/// The lock is the same one huggingface_hub takes on this path: its `filelock`
/// dependency locks it with `fcntl.flock`, which is what `File::lock` and
/// `File::try_lock` use, so the two contend for it (verified against filelock
/// 4.x, whose `UnixFileLock` calls `fcntl.flock`).
const SHARED_BLOB_LOCK_WAIT: Duration = Duration::from_secs(2);

/// How much of a `.refs` manifest one sweep reads. Real manifests list one short
/// path per sharing repo; 1 MiB is orders above any of them.
const SHARED_BLOB_REFS_MAX: u64 = 1 << 20;

/// The shared-store file a repo `blobs/<sha>` symlink points at, or `None` when
/// `entry` is a regular blob or the target is not a well-formed store path.
fn shared_store_target(entry: &Path, cache_root: Option<&Path>) -> Option<PathBuf> {
  shared_store_target_checked(entry, cache_root?)
    .ok()
    .flatten()
}

/// [`shared_store_target`] with filesystem faults kept: a caller about to delete
/// a payload has to treat "this reference is gone" and "I could not read this
/// reference" differently.
fn shared_store_target_checked(entry: &Path, cache_root: &Path) -> io::Result<Option<PathBuf>> {
  match shared_store_dir(cache_root)? {
    Some(store_dir) => store_target(entry, &store_dir),
    None => Ok(None),
  }
}

/// The cache's shared-blob store directory, or `None` when this cache has no
/// marked store. `Err` when the cache root itself cannot be read.
fn shared_store_dir(cache_root: &Path) -> io::Result<Option<PathBuf>> {
  let store_dir = cache_root.canonicalize()?.join("blobs");
  if !store_dir.join(SHARED_BLOBS_MARKER).is_file() {
    return Ok(None);
  }
  Ok(Some(store_dir))
}

/// The entry of `store_dir` that `entry`'s symlink resolves to. Split out so a
/// caller walking a whole manifest resolves the store once instead of
/// re-canonicalising the cache root and re-statting the marker per line.
fn store_target(entry: &Path, store_dir: &Path) -> io::Result<Option<PathBuf>> {
  let Some(target) = link_target_one_hop(entry)? else {
    return Ok(None);
  };
  Ok(store_shape(&target, store_dir).then_some(target))
}

/// `<cache>/blobs/<xx>/<hash>` with one lowercase 64-hex hash, huggingface_hub's
/// store shape. Anything else is not ours to touch.
fn store_shape(target: &Path, store_dir: &Path) -> bool {
  let Some(name) = target.file_name().and_then(|n| n.to_str()) else {
    return false;
  };
  let Some(prefix) = target.parent() else {
    return false;
  };
  let hash = name.len() == 64
    && name
      .bytes()
      .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
  hash
    && prefix.parent() == Some(store_dir)
    && prefix.file_name().and_then(|p| p.to_str()) == Some(&name[..2])
}

/// Remove a shared-store file once no repo links to it, the way huggingface_hub's
/// own cache delete does: under its `.lock`, every `.refs` line is checked against
/// the filesystem. Anything that cannot be checked keeps the file, so a cache
/// problem never costs bytes another model still needs.
fn sweep_shared_blob(store: &Path, cache_root: &Path, locks_close: Instant) {
  if Instant::now() >= locks_close {
    return;
  }
  let Ok(cache_root) = cache_root.canonicalize() else {
    return;
  };
  let store_dir = match shared_store_dir(&cache_root) {
    Ok(Some(store_dir)) => store_dir,
    // No marked store, or a cache root we cannot read: nothing to check, so
    // nothing to free.
    _ => return,
  };
  // Belt and braces on a path we are about to unlink: only ever a well-formed
  // entry of this cache's own store.
  if !store_shape(store, &store_dir) {
    return;
  }
  let Some(name) = store.file_name().and_then(|n| n.to_str()) else {
    return;
  };
  let lock_path = store.with_file_name(format!("{name}.lock"));
  let refs_path = store.with_file_name(format!("{name}.refs"));
  // Two guards on the lock path, because locking it means creating it: a
  // symlink here would have us lock (and create) whatever it points at. The
  // pre-check refuses a non-regular path, and `O_NOFOLLOW` closes the gap where
  // a link is planted between that check and this open.
  if fs::symlink_metadata(&lock_path).is_ok_and(|m| !m.is_file()) {
    return;
  }
  let mut opts = fs::OpenOptions::new();
  opts.read(true).write(true).create(true).truncate(false);
  #[cfg(unix)]
  opts.custom_flags(libc::O_NOFOLLOW);
  let Ok(lock) = opts.open(&lock_path) else {
    return;
  };
  if !lock_blob(&lock, locks_close) {
    return;
  }
  // A regular file or nothing: a symlinked manifest would have us follow it
  // wherever it points.
  if !fs::symlink_metadata(&refs_path).is_ok_and(|m| m.is_file()) {
    return;
  }
  let Ok(mut manifest) = fs::File::open(&refs_path) else {
    return;
  };
  // A manifest is one short line per repo linking the payload. Read at most one
  // more than that allows: past it the file is corrupt or planted, and reading
  // it whole would cost memory on the thread running the delete.
  let mut raw = Vec::new();
  let Ok(read) = manifest
    .by_ref()
    .take(SHARED_BLOB_REFS_MAX + 1)
    .read_to_end(&mut raw)
  else {
    return;
  };
  if read as u64 > SHARED_BLOB_REFS_MAX {
    log::debug!("delete: shared blob manifest too large, leaving the payload");
    return;
  }
  let Ok(refs) = String::from_utf8(raw) else {
    return;
  };
  for line in refs.lines() {
    // A blank line says nothing about a reference and is not grounds to keep.
    if line.is_empty() {
      continue;
    }
    let rel = Path::new(line);
    // A line outside the manifest's contract (absolute, or containing `..` or a
    // path prefix) cannot be checked against this cache, so it keeps the payload
    // rather than letting a sweep run on a list it only partly read.
    let checkable = rel.is_relative()
      && rel
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    if !checkable {
      log::debug!("delete: shared blob manifest lists `{line}`, leaving the payload");
      return;
    }
    let still_links = match store_target(&cache_root.join(rel), &store_dir) {
      Err(_) => return,
      Ok(Some(target)) => target.as_path() == store,
      Ok(None) => false,
    };
    if still_links {
      return;
    }
  }
  if fs::symlink_metadata(store).is_ok_and(|m| m.is_file()) {
    let _ = fs::remove_file(store);
    let _ = fs::remove_file(&refs_path);
  }
}

/// Take the store's advisory lock, giving up once this delete's [`locks_close`]
/// deadline runs out. `File::lock` waits forever, and the caller is whichever
/// thread asked for the delete: for the TUI that is the thread drawing the
/// screen.
fn lock_blob(lock: &fs::File, locks_close: Instant) -> bool {
  loop {
    match lock.try_lock() {
      Ok(()) => return true,
      // Contended: worth another pass while this delete's budget lasts.
      Err(std::fs::TryLockError::WouldBlock) => {}
      // A lock that cannot be taken at all is not worth waiting on.
      Err(_) => return false,
    }
    if Instant::now() >= locks_close {
      log::debug!("delete: blob locks busy, leaving the remaining payloads for now");
      return false;
    }
    std::thread::sleep(Duration::from_millis(25));
  }
}

/// The `models--<owner>--<repo>` directory `path` sits under by directory
/// shape alone, with no cache-root gate and no canonicalisation. Pure path
/// arithmetic, so two paths from the same discovery pass compare reliably —
/// that is all the "is anything else still in this repo" question needs.
///
/// Three row shapes resolve to the same repo: a GGUF **file**
/// (`models--*/snapshots/<rev>/<file>`), the same file nested in a quant
/// **subdirectory** (`.../snapshots/<rev>/Q4_K_M/<file>`, which the recursive
/// GGUF scanner emits as its own row), and a whole-snapshot **directory**
/// (`models--*/snapshots/<rev>`), which is what a safetensors row carries.
/// All must land on the same answer or the "last model in this repo" check
/// would treat a mixed catalog as unrelated repos — and a nested GGUF that
/// resolved to `None` used to be invisible to that check, so deleting the
/// directory row planned a whole-repo `remove_dir_all` straight over it.
fn hf_repo_dir_shape(path: &Path) -> Option<&Path> {
  // Walk up to `snapshots/` from any depth rather than testing fixed levels.
  let mut cursor = path;
  while let Some(parent) = cursor.parent() {
    if parent.file_name().and_then(|n| n.to_str()) == Some("snapshots") {
      let repo_dir = parent.parent()?;
      return repo_dir
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("models--"))
        .then_some(repo_dir);
    }
    cursor = parent;
  }
  None
}

/// [`hf_repo_dir_shape`] plus the gate that makes recursive removal safe: the
/// resolved repo directory must live inside `cache_root`. An HF-shaped tree
/// somewhere else — an rsynced backup, a restored archive, a surprise Docker
/// volume — resolves to `None` and takes the per-file path instead.
fn hf_repo_dir_in_cache(path: &Path, cache_root: Option<&Path>) -> Option<PathBuf> {
  let repo_dir = hf_repo_dir_shape(path)?;
  let cache_root = cache_root?;
  let cache_root_canonical = cache_root
    .canonicalize()
    .unwrap_or_else(|_| cache_root.to_path_buf());
  let candidate = repo_dir
    .canonicalize()
    .unwrap_or_else(|_| repo_dir.to_path_buf());
  candidate
    .starts_with(&cache_root_canonical)
    .then_some(candidate)
}

/// `["a"] -> "a"`, `["a", "b"] -> "a and b"`, `["a", "b", "c"] -> "a, b and c"`.
fn join_human(parts: &[String]) -> String {
  match parts {
    [] => String::new(),
    [one] => one.clone(),
    [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::discovery::ModelSource;
  use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};

  fn model(path: &Path) -> DiscoveredModel {
    DiscoveredModel {
      path: path.to_path_buf(),
      parent: path.parent().unwrap_or(Path::new("/")).to_path_buf(),
      source: ModelSource::UserPath,
      metadata: None,
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: None,
      multimodal: None,
      supported_backends: vec!["llamacpp".into()],
      mtp_head: None,
    }
  }

  fn with_arch(mut m: DiscoveredModel, arch: &str) -> DiscoveredModel {
    m.metadata = Some(ModelMetadata {
      arch: Some(arch.to_string()),
      total_parameters: None,
      parameter_label: None,
      quant: Quant::Unknown(0),
      quant_label: None,
      native_ctx: None,
      chat_template: None,
      tokenizer_kind: None,
      reasoning_hint: false,
      mode_hint: ModeHint::Unknown,
      weights_bytes: None,
      lazy_tensor_bytes: Vec::new(),
      mtp: None,
    });
    m
  }

  fn tempdir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
      "llamastash-delete-{label}-{}-{:?}",
      std::process::id(),
      std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    // `hf_repo_dir_in_cache` returns a canonical path, so expectations built
    // from this root must be canonical too. `temp_dir()` is a symlink on macOS
    // (`/var` -> `/private/var`) and an 8.3 short name on Windows; only Linux
    // hands back a path that already compares equal.
    dir.canonicalize().unwrap_or(dir)
  }

  #[test]
  fn plan_takes_every_shard_of_a_split_set() {
    let dir = tempdir("shards");
    let shard1 = dir.join("big-00001-of-00003.gguf");
    let shard2 = dir.join("big-00002-of-00003.gguf");
    let shard3 = dir.join("big-00003-of-00003.gguf");
    for p in [&shard1, &shard2, &shard3] {
      fs::write(p, b"weights").unwrap();
    }
    let mut target = model(&shard1);
    target.split_siblings = vec![shard2.clone(), shard3.clone()];

    let plan = plan(&target, std::slice::from_ref(&target), None);
    assert_eq!(plan.files().len(), 3);
    let summary = execute(&plan).expect("delete must succeed");
    assert!(summary.contains("companion files"), "got `{summary}`");
    for p in [&shard1, &shard2, &shard3] {
      assert!(!p.exists(), "{} must be gone", p.display());
    }
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn plan_takes_projector_and_mtp_head_of_a_lone_model() {
    // The catalog row carries no capability flags here on purpose: these
    // companion files have unparseable headers, so discovery left
    // `multimodal` / `mtp_head` unset. They are still this model's files.
    let dir = tempdir("companions");
    let gguf = dir.join("gemma-4-Q4_K_M.gguf");
    let proj = dir.join("mmproj-gemma-4-f16.gguf");
    let head = dir.join("mtp-gemma-4.gguf");
    for p in [&gguf, &proj, &head] {
      fs::write(p, b"weights").unwrap();
    }
    let target = model(&gguf);

    let plan = plan(&target, std::slice::from_ref(&target), None);
    assert_eq!(plan.projector.as_deref(), Some(proj.as_path()));
    assert_eq!(plan.mtp_head.as_deref(), Some(head.as_path()));
    execute(&plan).expect("delete must succeed");
    for p in [&gguf, &proj, &head] {
      assert!(!p.exists(), "{} must be gone", p.display());
    }
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn plan_takes_the_unpaired_projector_precisions_with_the_model() {
    // unsloth ships one projector at two precisions. Only the ranked one pairs,
    // so the other is orphaned the moment this model goes — it must not survive
    // as an unreferenced file next to an empty folder.
    let dir = tempdir("companions-two-precisions");
    let gguf = dir.join("Qwen3.8-27B-UD-Q6_K.gguf");
    let paired = dir.join("mmproj-F16.gguf");
    let orphan = dir.join("mmproj-BF16.gguf");
    for p in [&gguf, &paired, &orphan] {
      fs::write(p, b"weights").unwrap();
    }
    let target = model(&gguf);

    let plan = plan(&target, std::slice::from_ref(&target), None);
    assert_eq!(plan.projector.as_deref(), Some(paired.as_path()));
    assert_eq!(plan.orphan_companions, vec![orphan.clone()]);
    execute(&plan).expect("delete must succeed");
    for p in [&gguf, &paired, &orphan] {
      assert!(!p.exists(), "{} must be gone", p.display());
    }
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn plan_keeps_a_nameless_projector_another_model_pairs_with() {
    // A nameless file is only a leftover when the paired companion is nameless
    // too. Here alpha pairs with its own named projector, and the nameless
    // mmproj.gguf is what beta pairs with — deleting alpha must not take it.
    let dir = tempdir("named-plus-nameless");
    let a = dir.join("alpha-Q4_K_M.gguf");
    let b = dir.join("beta-Q4_K_M.gguf");
    let named = dir.join("mmproj-alpha-BF16.gguf");
    let nameless = dir.join("mmproj.gguf");
    for p in [&a, &b, &named, &nameless] {
      fs::write(p, b"weights").unwrap();
    }
    let target = model(&a);
    let catalog = vec![target.clone(), model(&b)];

    let plan = plan(&target, &catalog, None);
    assert_eq!(plan.projector.as_deref(), Some(named.as_path()));
    assert!(
      plan.orphan_companions.is_empty(),
      "beta pairs with {:?}, so it is no leftover",
      plan.orphan_companions
    );
    execute(&plan).expect("delete must succeed");
    assert!(nameless.exists(), "the surviving model keeps its projector");
    assert!(b.exists());
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn plan_keeps_a_companion_another_model_still_pairs_with() {
    // Two models sharing one anonymous projector: `find_mmproj`'s catch-all
    // rule hands the same file to both, so deleting one must leave it.
    let dir = tempdir("shared-proj");
    let a = dir.join("alpha-Q4_K_M.gguf");
    let b = dir.join("beta-Q4_K_M.gguf");
    let proj = dir.join("mmproj.gguf");
    for p in [&a, &b, &proj] {
      fs::write(p, b"weights").unwrap();
    }
    let target = model(&a);
    let catalog = vec![target.clone(), model(&b)];

    let plan = plan(&target, &catalog, None);
    assert_eq!(plan.projector, None, "shared projector must survive");
    execute(&plan).expect("delete must succeed");
    assert!(!a.exists());
    assert!(
      proj.exists(),
      "the surviving model still needs its projector"
    );
    assert!(b.exists());
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn plan_removes_whole_hf_repo_when_it_holds_the_last_model() {
    let cache_root = tempdir("hf-last");
    let repo_dir = cache_root.join("models--owner--repo");
    let blobs = repo_dir.join("blobs");
    let snap = repo_dir.join("snapshots").join("main");
    fs::create_dir_all(&blobs).unwrap();
    fs::create_dir_all(&snap).unwrap();
    fs::write(blobs.join("sha"), b"blob").unwrap();
    let file = snap.join("file.gguf");
    symlink_or_copy(&blobs.join("sha"), &file);

    let target = model(&file);
    let plan = plan(&target, std::slice::from_ref(&target), Some(&cache_root));
    assert!(plan.hf_repo_dir.is_some());
    let summary = execute(&plan).expect("delete must succeed");
    assert!(summary.contains("HF cache"), "got `{summary}`");
    assert!(!repo_dir.exists(), "the whole repo dir should be gone");
    let _ = fs::remove_dir_all(&cache_root);
  }

  #[test]
  fn plan_spares_a_second_quant_sharing_the_hf_repo() {
    // Two quants pulled from one repo land in the same `models--*` dir.
    // Deleting one must take its blob and leave the other runnable.
    let cache_root = tempdir("hf-two-quants");
    let repo_dir = cache_root.join("models--owner--repo");
    let blobs = repo_dir.join("blobs");
    let snap = repo_dir.join("snapshots").join("main");
    fs::create_dir_all(&blobs).unwrap();
    fs::create_dir_all(&snap).unwrap();
    fs::write(blobs.join("sha-q4"), b"q4").unwrap();
    fs::write(blobs.join("sha-q8"), b"q8").unwrap();
    let q4 = snap.join("model-Q4_K_M.gguf");
    let q8 = snap.join("model-Q8_0.gguf");
    symlink_or_copy(&blobs.join("sha-q4"), &q4);
    symlink_or_copy(&blobs.join("sha-q8"), &q8);

    let target = model(&q4);
    let catalog = vec![target.clone(), model(&q8)];
    let plan = plan(&target, &catalog, Some(&cache_root));
    assert_eq!(
      plan.hf_repo_dir, None,
      "a repo with a second model must not be nuked wholesale"
    );
    execute(&plan).expect("delete must succeed");
    assert!(!q4.exists(), "the deleted snapshot link must be gone");
    assert!(q8.exists(), "the other quant must survive");
    assert!(blobs.join("sha-q8").exists(), "its blob must survive too");
    #[cfg(unix)]
    assert!(
      !blobs.join("sha-q4").exists(),
      "the deleted model's blob must be reclaimed"
    );
    let _ = fs::remove_dir_all(&cache_root);
  }

  #[test]
  fn hf_shaped_tree_outside_the_cache_root_only_unlinks() {
    let outside = tempdir("hf-outside");
    let repo_dir = outside.join("models--owner--repo");
    let snap = repo_dir.join("snapshots").join("main");
    fs::create_dir_all(&snap).unwrap();
    let file = snap.join("file.gguf");
    let other = snap.join("other.gguf");
    fs::write(&file, b"weights").unwrap();
    fs::write(&other, b"other").unwrap();
    let unrelated_cache = tempdir("hf-unrelated");

    let target = model(&file);
    let plan = plan(
      &target,
      std::slice::from_ref(&target),
      Some(&unrelated_cache),
    );
    assert_eq!(plan.hf_repo_dir, None);
    let summary = execute(&plan).expect("delete must succeed");
    assert!(!summary.contains("HF cache"), "got `{summary}`");
    assert!(!file.exists());
    assert!(other.exists(), "sibling file must not be removed");
    assert!(repo_dir.exists(), "the repo dir must not be removed");
    let _ = fs::remove_dir_all(&outside);
    let _ = fs::remove_dir_all(&unrelated_cache);
  }

  #[test]
  fn no_cache_root_never_recurses() {
    let dir = tempdir("no-cache-root");
    let repo_dir = dir.join("models--owner--repo");
    let snap = repo_dir.join("snapshots").join("main");
    fs::create_dir_all(&snap).unwrap();
    let file = snap.join("file.gguf");
    fs::write(&file, b"weights").unwrap();

    let target = model(&file);
    let plan = plan(&target, std::slice::from_ref(&target), None);
    assert_eq!(plan.hf_repo_dir, None);
    execute(&plan).expect("delete must succeed");
    assert!(!file.exists());
    assert!(repo_dir.exists());
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn missing_companion_does_not_fail_the_delete() {
    let dir = tempdir("missing-companion");
    let gguf = dir.join("m.gguf");
    fs::write(&gguf, b"weights").unwrap();
    let mut target = model(&gguf);
    target.split_siblings = vec![dir.join("m-vanished.gguf")];

    let plan = plan(&target, std::slice::from_ref(&target), None);
    let summary = execute(&plan).expect("a vanished sibling must not fail the delete");
    assert!(summary.contains("m.gguf"), "got `{summary}`");
    assert!(!gguf.exists());
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn arch_matched_mtp_head_is_kept_when_a_sibling_model_also_drafts_from_it() {
    // Both models declare `deepseek4`, so the head's `deepseek4_mtp_support`
    // arch pairs with either. Deleting one must leave the head for the other.
    use crate::gguf::test_fixtures::FixtureBuilder;
    let dir = tempdir("shared-head");
    let a = dir.join("deepseek-v4-flash-Q2_K.gguf");
    let b = dir.join("deepseek-v4-pro-Q2_K.gguf");
    let head = dir.join("mtp-deepseek-v4.gguf");
    fs::write(&a, b"weights").unwrap();
    fs::write(&b, b"weights").unwrap();
    fs::write(
      &head,
      FixtureBuilder::new()
        .with_arch("deepseek4_mtp_support")
        .build(),
    )
    .unwrap();

    let target = with_arch(model(&a), "deepseek4");
    let catalog = vec![target.clone(), with_arch(model(&b), "deepseek4")];

    let plan = plan(&target, &catalog, None);
    assert_eq!(plan.mtp_head, None, "shared draft head must survive");
    execute(&plan).expect("delete must succeed");
    assert!(head.exists());
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn describe_names_the_companions_going_with_the_model() {
    let plan = DeletePlan {
      primary: PathBuf::from("/m/a-00001-of-00002.gguf"),
      shards: vec![PathBuf::from("/m/a-00002-of-00002.gguf")],
      projector: Some(PathBuf::from("/m/mmproj-a.gguf")),
      mtp_head: Some(PathBuf::from("/m/mtp-a.gguf")),
      ..DeletePlan::default()
    };
    let body = plan.describe("a");
    assert!(body.contains("4 files"), "got `{body}`");
    assert!(body.contains("1 more shard"), "got `{body}`");
    assert!(body.contains("mmproj projector"), "got `{body}`");
    assert!(body.contains("MTP draft head"), "got `{body}`");
    assert!(
      !body.contains('\n'),
      "the overlay renders one wrapped Line; a newline collapses mid-sentence"
    );
    assert!(
      !body.contains("blobs"),
      "a plain user path has no cache blobs to promise: `{body}`"
    );
  }

  #[test]
  fn describe_single_file_stays_short() {
    let plan = DeletePlan::single("/m/a.gguf");
    let body = plan.describe("a");
    assert!(body.contains("One file"), "got `{body}`");
  }

  #[test]
  fn describe_mentions_blobs_only_inside_the_hf_cache() {
    let cache_root = tempdir("describe-blobs");
    let snap = cache_root.join("models--owner--repo/snapshots/main");
    fs::create_dir_all(&snap).unwrap();
    let a = snap.join("a.gguf");
    let b = snap.join("b.gguf");
    fs::write(&a, b"a").unwrap();
    fs::write(&b, b"b").unwrap();
    let target = model(&a);
    // A second model in the repo keeps this on the per-file path, which is
    // the branch whose copy mentions blobs.
    let catalog = vec![target.clone(), model(&b)];
    let body = plan(&target, &catalog, Some(&cache_root)).describe("a");
    assert!(body.contains("blobs"), "got `{body}`");
    let _ = fs::remove_dir_all(&cache_root);
  }

  #[test]
  fn describe_whole_repo_names_the_repo() {
    let plan = DeletePlan {
      primary: PathBuf::from("/c/models--owner--repo/snapshots/main/a.gguf"),
      hf_repo_dir: Some(PathBuf::from("/c/models--owner--repo")),
      ..DeletePlan::default()
    };
    let body = plan.describe("a");
    assert!(body.contains("models--owner--repo"), "got `{body}`");
  }

  /// A catalog row whose `path` is a whole snapshot directory rather than a
  /// single file — the shape a safetensors backend produces. These four pin
  /// that the file-shaped assumptions in this module degrade safely.
  #[test]
  fn directory_row_resolves_no_companions() {
    let root = tempdir("dir-row-companions");
    let snapshot = root.join("models--o--r/snapshots/rev");
    fs::create_dir_all(&snapshot).unwrap();
    fs::write(snapshot.join("model.safetensors"), b"w").unwrap();
    // An mmproj sitting where the file-shaped finder would look.
    fs::write(root.join("models--o--r/snapshots/mmproj-x.gguf"), b"p").unwrap();

    let target = model(&snapshot);
    let p = plan(&target, std::slice::from_ref(&target), None);
    assert_eq!(p.projector, None, "a directory row owns no projector");
    assert_eq!(p.mtp_head, None, "a directory row owns no MTP head");
    assert!(p.shards.is_empty());
    let _ = fs::remove_dir_all(&root);
  }

  #[test]
  fn directory_row_last_in_repo_plans_whole_repo_removal() {
    let cache_root = tempdir("dir-row-repo");
    let repo = cache_root.join("models--o--r");
    let snapshot = repo.join("snapshots/rev");
    fs::create_dir_all(&snapshot).unwrap();

    let target = model(&snapshot);
    let p = plan(&target, std::slice::from_ref(&target), Some(&cache_root));
    assert_eq!(p.hf_repo_dir.as_deref(), Some(repo.as_path()));
    let _ = fs::remove_dir_all(&cache_root);
  }

  #[test]
  fn directory_row_sharing_a_repo_does_not_plan_repo_removal() {
    let cache_root = tempdir("dir-row-shared-repo");
    let repo = cache_root.join("models--o--r");
    let snapshot = repo.join("snapshots/rev");
    fs::create_dir_all(&snapshot).unwrap();
    let sibling = repo.join("snapshots/rev/other.gguf");

    let target = model(&snapshot);
    let catalog = vec![target.clone(), model(&sibling)];
    let p = plan(&target, &catalog, Some(&cache_root));
    assert_eq!(
      p.hf_repo_dir, None,
      "another model still lives in the repo, so the repo must survive"
    );
    let _ = fs::remove_dir_all(&cache_root);
  }

  /// A GGUF nested one level below the snapshot dir is a real catalog row
  /// (the scanner's walk is recursive). It used to resolve to a `None` repo
  /// shape, so the exclusivity check missed it and deleting the safetensors
  /// directory row planned `remove_dir_all` over the whole repo — destroying
  /// a model the user did not select, behind a prompt saying it was the last.
  #[test]
  fn a_nested_gguf_keeps_its_repo_alive() {
    let cache_root = tempdir("dir-row-nested-gguf");
    let repo = cache_root.join("models--o--r");
    let snapshot = repo.join("snapshots/rev");
    fs::create_dir_all(snapshot.join("Q4_K_M")).unwrap();
    let nested = snapshot.join("Q4_K_M/model.gguf");
    fs::write(&nested, b"g").unwrap();

    assert_eq!(
      hf_repo_dir_shape(&nested),
      Some(repo.as_path()),
      "a nested GGUF must resolve to the same repo as its snapshot dir"
    );

    let target = model(&snapshot);
    let catalog = vec![target.clone(), model(&nested)];
    let p = plan(&target, &catalog, Some(&cache_root));
    assert_eq!(
      p.hf_repo_dir, None,
      "the nested GGUF still lives here, so the repo must survive"
    );
    let _ = fs::remove_dir_all(&cache_root);
  }

  /// Outside the resolved cache root nothing else shares the directory, so
  /// the delete is exactly what the user asked for. This used to refuse with
  /// "is not the only model in its cache repo" — a false reason that left the
  /// row with no route to removal at all.
  #[test]
  fn a_directory_outside_the_cache_root_is_deleted() {
    let root = tempdir("dir-row-outside-cache");
    let snapshot = root.join("snapshots/rev");
    fs::create_dir_all(&snapshot).unwrap();
    fs::write(snapshot.join("model.safetensors"), b"w").unwrap();

    let msg = execute(&DeletePlan::single(&snapshot)).expect("must delete");
    assert!(msg.contains("deleted"), "got `{msg}`");
    assert!(!snapshot.exists(), "the directory must be gone");
    let _ = fs::remove_dir_all(&root);
  }

  /// Inside the cache with a sibling still listed, the repo arm is withheld
  /// and the refusal must state *that* reason.
  #[test]
  fn a_shared_cache_repo_directory_is_refused_with_the_real_reason() {
    let cache_root = tempdir("dir-row-shared-refuse");
    let repo = cache_root.join("models--o--r");
    let snapshot = repo.join("snapshots/rev");
    fs::create_dir_all(&snapshot).unwrap();
    fs::write(snapshot.join("model.safetensors"), b"w").unwrap();
    let sibling = repo.join("snapshots/rev2/other.gguf");
    fs::create_dir_all(sibling.parent().unwrap()).unwrap();
    fs::write(&sibling, b"g").unwrap();

    let target = model(&snapshot);
    let catalog = vec![target.clone(), model(&sibling)];
    let p = plan(&target, &catalog, Some(&cache_root));
    assert_eq!(
      p.hf_repo_dir, None,
      "a sibling survives, so no repo removal"
    );

    let err = execute(&p).expect_err("must refuse");
    assert!(
      err
        .to_string()
        .contains("not the only model in its cache repo"),
      "got `{err}`"
    );
    assert!(
      snapshot.exists(),
      "nothing may be removed on the refusal path"
    );
    let _ = fs::remove_dir_all(&cache_root);
  }

  /// huggingface_hub's shared-store layout: the payload in
  /// `<cache>/blobs/<xx>/<hash>` with a `.refs` listing each repo symlink to it,
  /// `<repo>/blobs/<etag>` a relative link to that, and the snapshot file a
  /// relative link to the repo entry. Returns (snapshot file, store payload).
  #[cfg(unix)]
  fn shared_store_model(
    cache_root: &Path,
    repo: &str,
    file: &str,
    hash_byte: char,
  ) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::symlink;
    let store_dir = cache_root.join("blobs");
    fs::create_dir_all(&store_dir).unwrap();
    fs::write(store_dir.join(SHARED_BLOBS_MARKER), b"1").unwrap();
    let hash: String = std::iter::repeat_n(hash_byte, 64).collect();
    let prefix = store_dir.join(&hash[..2]);
    fs::create_dir_all(&prefix).unwrap();
    let payload = prefix.join(&hash);
    if !payload.exists() {
      fs::write(&payload, b"weights").unwrap();
    }
    let etag = format!("etag-{file}");
    let repo_blobs = cache_root.join(repo).join("blobs");
    let snap = cache_root.join(repo).join("snapshots/rev");
    fs::create_dir_all(&repo_blobs).unwrap();
    fs::create_dir_all(&snap).unwrap();
    symlink(
      format!("../../blobs/{}/{hash}", &hash[..2]),
      repo_blobs.join(&etag),
    )
    .unwrap();
    let mut refs =
      fs::read_to_string(payload.with_file_name(format!("{hash}.refs"))).unwrap_or_default();
    refs.push_str(&format!("{repo}/blobs/{etag}\n"));
    fs::write(payload.with_file_name(format!("{hash}.refs")), refs).unwrap();
    let link = snap.join(file);
    symlink(format!("../../blobs/{etag}"), &link).unwrap();
    (link, payload)
  }

  #[cfg(unix)]
  #[test]
  fn last_model_in_repo_frees_its_shared_store_payload() {
    let cache_root = tempdir("shared-last");
    let (file, payload) = shared_store_model(&cache_root, "models--o--r", "m.gguf", 'a');
    let target = model(&file);
    let p = plan(&target, std::slice::from_ref(&target), Some(&cache_root));
    assert!(p.hf_repo_dir.is_some());
    execute(&p).expect("delete must succeed");
    assert!(!cache_root.join("models--o--r").exists());
    assert!(!payload.exists(), "the shared payload must be freed");
    assert!(!payload.with_extension("refs").exists());
    let _ = fs::remove_dir_all(&cache_root);
  }

  #[cfg(unix)]
  #[test]
  fn one_quant_of_two_frees_only_its_shared_store_payload() {
    let cache_root = tempdir("shared-two-quants");
    let (q4, q4_payload) = shared_store_model(&cache_root, "models--o--r", "q4.gguf", 'b');
    let (q8, q8_payload) = shared_store_model(&cache_root, "models--o--r", "q8.gguf", 'c');
    let target = model(&q4);
    let catalog = vec![target.clone(), model(&q8)];
    let p = plan(&target, &catalog, Some(&cache_root));
    assert_eq!(p.hf_repo_dir, None);
    execute(&p).expect("delete must succeed");
    assert!(
      !q4_payload.exists(),
      "the deleted quant's payload must be freed"
    );
    assert!(
      q8_payload.exists(),
      "the other quant's payload must survive"
    );
    assert!(fs::read(&q8).is_ok(), "the other quant must still resolve");
    let _ = fs::remove_dir_all(&cache_root);
  }

  #[cfg(unix)]
  #[test]
  fn shared_store_payload_linked_from_another_repo_survives() {
    let cache_root = tempdir("shared-two-repos");
    let (a, payload) = shared_store_model(&cache_root, "models--o--a", "m.gguf", 'd');
    let (b, _) = shared_store_model(&cache_root, "models--o--b", "m.gguf", 'd');
    let target = model(&a);
    let catalog = vec![target.clone(), model(&b)];
    execute(&plan(&target, &catalog, Some(&cache_root))).expect("delete must succeed");
    assert!(!cache_root.join("models--o--a").exists());
    assert!(payload.exists(), "repo b still links the payload");
    assert!(fs::read(&b).is_ok());
    let _ = fs::remove_dir_all(&cache_root);
  }

  /// huggingface_hub's rule: metadata that cannot be read is never grounds for
  /// deletion. Here the manifest path is occupied by a directory, so reading it
  /// fails rather than listing nothing.
  #[cfg(unix)]
  #[test]
  fn a_shared_store_payload_with_an_unreadable_refs_survives() {
    let cache_root = tempdir("shared-bad-refs");
    let (file, payload) = shared_store_model(&cache_root, "models--o--r", "m.gguf", 'e');
    let refs = payload.with_extension("refs");
    fs::remove_file(&refs).unwrap();
    fs::create_dir(&refs).unwrap();
    let target = model(&file);
    execute(&plan(
      &target,
      std::slice::from_ref(&target),
      Some(&cache_root),
    ))
    .expect("delete must succeed");
    assert!(!cache_root.join("models--o--r").exists());
    assert!(payload.exists(), "an unreadable .refs keeps the payload");
    let _ = fs::remove_dir_all(&cache_root);
  }

  /// A lock someone else holds leaves the payload: the sweep waits at most this
  /// delete's lock budget, then moves on. It burns the whole budget on purpose,
  /// so it takes about two seconds.
  #[cfg(unix)]
  #[test]
  fn a_busy_shared_blob_lock_leaves_the_payload() {
    let cache_root = tempdir("shared-lock-busy");
    let (file, payload) = shared_store_model(&cache_root, "models--o--r", "m.gguf", 'e');
    let holder = fs::OpenOptions::new()
      .read(true)
      .write(true)
      .create(true)
      .truncate(false)
      .open(payload.with_extension("lock"))
      .unwrap();
    // A second handle in this process is a second lock, the way another
    // process's flock would be.
    holder.try_lock().expect("take the lock first");
    let target = model(&file);
    execute(&plan(
      &target,
      std::slice::from_ref(&target),
      Some(&cache_root),
    ))
    .expect("delete must succeed");
    assert!(payload.exists(), "a busy lock keeps the payload");
    drop(holder);
    let _ = fs::remove_dir_all(&cache_root);
  }

  /// A manifest line outside the contract (huggingface_hub writes cache-relative
  /// posix paths) is a list the sweep cannot check, so it keeps the payload
  /// instead of sweeping on a half-read reference set.
  #[cfg(unix)]
  #[test]
  fn an_absolute_shared_store_ref_keeps_the_payload() {
    let cache_root = tempdir("shared-ref-absolute");
    let (file, payload) = shared_store_model(&cache_root, "models--o--r", "m.gguf", '9');
    let abs = cache_root
      .join("models--o--r")
      .join("blobs")
      .join("etag-m.gguf");
    fs::write(
      payload.with_extension("refs"),
      format!("{}\n", abs.display()),
    )
    .unwrap();
    let target = model(&file);
    execute(&plan(
      &target,
      std::slice::from_ref(&target),
      Some(&cache_root),
    ))
    .expect("delete must succeed");
    assert!(!cache_root.join("models--o--r").exists());
    assert!(
      payload.exists(),
      "a reference the sweep cannot check keeps the payload"
    );
    let _ = fs::remove_dir_all(&cache_root);
  }

  /// A listed reference the sweep cannot read is not an absent one: a permission
  /// or path fault on the *last* line must not free a payload another repo may
  /// still link. Here the path runs through a regular file, so reading it fails
  /// for every user, root included.
  #[cfg(unix)]
  #[test]
  fn a_shared_store_ref_that_cannot_be_read_keeps_the_payload() {
    let cache_root = tempdir("shared-ref-fault");
    let (file, payload) = shared_store_model(&cache_root, "models--o--r", "m.gguf", 'f');
    fs::write(cache_root.join("blocker"), b"x").unwrap();
    fs::write(
      payload.with_extension("refs"),
      b"blocker/blobs/etag-m.gguf\n",
    )
    .unwrap();
    let target = model(&file);
    execute(&plan(
      &target,
      std::slice::from_ref(&target),
      Some(&cache_root),
    ))
    .expect("delete must succeed");
    assert!(!cache_root.join("models--o--r").exists());
    assert!(
      payload.exists(),
      "a reference that could not be checked keeps the payload"
    );
    let _ = fs::remove_dir_all(&cache_root);
  }

  fn symlink_or_copy(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(not(unix))]
    fs::copy(target, link).unwrap();
  }
}
