//! Copy-on-write trees.
//!
//! * A clone of a directory tree is made with `clonefile(2)` on macOS (APFS) or
//!   `FICLONE` reflinks on Linux (btrfs/xfs). Cloned files share disk blocks with the
//!   source, so a clone costs ~0 bytes until something is written, and creating one is a
//!   metadata operation.
//! * Top-level entries can be left out (`exclude`) or symlinked to the source (`link`,
//!   for heavy regenerable dirs when copy-on-write is not available).
//! * Cloning preserves mtimes, so a [`Manifest`] of (size, mtime) taken right after the
//!   clone tells exactly which files were touched later, without hashing.
//! * Changes flow back with a three-way merge (base, ours = target, theirs = clone).

use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneMethod {
    /// Whole subtree cloned in one syscall (APFS clonefile).
    CowTree,
    /// Per-file reflinks or copies (reflink when the filesystem supports it).
    PerFile,
}

/// File stamps of a tree right after it was cloned, keyed by `/`-separated relative path.
#[derive(Debug, Clone, Default)]
pub struct Manifest(pub BTreeMap<String, Stamp>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub len: u64,
    pub mtime: Option<SystemTime>,
}

impl Stamp {
    pub fn of(path: &Path) -> Option<Stamp> {
        let md = std::fs::symlink_metadata(path).ok()?;
        md.is_file().then(|| Stamp { len: md.len(), mtime: md.modified().ok() })
    }
}

#[derive(Debug, Clone)]
pub struct Rules {
    pub link: Vec<String>,
    pub exclude: Vec<String>,
}

/// Clones `src` into `dst` (which must not exist) and returns the manifest of `dst`.
pub fn clone_tree(src: &Path, dst: &Path, rules: &Rules) -> Result<(Manifest, CloneMethod)> {
    let method = copy_tree(src, dst, rules)?;
    Ok((manifest(dst, rules), method))
}

/// Clones `src` into `dst` like [`clone_tree`], without taking a manifest (walking every
/// cloned file is the slow part on big trees, and only mission sandboxes need it).
pub fn copy_tree(src: &Path, dst: &Path, rules: &Rules) -> Result<CloneMethod> {
    std::fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
    let mut method = CloneMethod::CowTree;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if rules.exclude.iter().any(|e| e == &name) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if rules.link.iter().any(|l| l == &name) {
            // Resolve so a sandbox cloned from a sandbox still points at the real dir.
            let target = std::fs::canonicalize(&from).unwrap_or(from);
            symlink(&target, &to)?;
            continue;
        }
        if !clone_cow(&from, &to) {
            method = CloneMethod::PerFile;
            copy_recursive(&from, &to)?;
        }
    }
    Ok(method)
}

/// Walks a sandbox and records the stamp of every regular file (symlinks are skipped).
pub fn manifest(root: &Path, rules: &Rules) -> Manifest {
    let mut m = Manifest::default();
    walk_files(root, rules, &mut |rel, abs| {
        if let Some(s) = Stamp::of(abs) {
            m.0.insert(rel.to_string(), s);
        }
    });
    m
}

/// Calls `f(relative path, absolute path)` for every regular file under `root` (top-level
/// excluded and linked entries skipped).
pub fn for_each_file(root: &Path, rules: &Rules, f: &mut dyn FnMut(&str, &Path)) {
    walk_files(root, rules, f)
}

/// Clones one file copy-on-write; false when the filesystem cannot.
pub fn clone_cow_file(from: &Path, to: &Path) -> bool {
    clone_cow(from, to) || reflink(from, to)
}

fn walk_files(root: &Path, rules: &Rules, f: &mut dyn FnMut(&str, &Path)) {
    fn rec(dir: &Path, rel: &str, top: bool, rules: &Rules, f: &mut dyn FnMut(&str, &Path)) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if top && (rules.exclude.contains(&name) || rules.link.contains(&name)) {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            let child_rel = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if ft.is_dir() {
                rec(&entry.path(), &child_rel, false, rules, f);
            } else if ft.is_file() {
                f(&child_rel, &entry.path());
            }
        }
    }
    rec(root, "", true, rules, f);
}

#[cfg(target_os = "macos")]
fn clone_cow(from: &Path, to: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    const CLONE_NOFOLLOW: u32 = 0x0001;
    let (Ok(a), Ok(b)) = (
        CString::new(from.as_os_str().as_bytes()),
        CString::new(to.as_os_str().as_bytes()),
    ) else {
        return false;
    };
    // SAFETY: both pointers are valid NUL-terminated strings for the duration of the call.
    unsafe { libc::clonefile(a.as_ptr(), b.as_ptr(), CLONE_NOFOLLOW) == 0 }
}

#[cfg(not(target_os = "macos"))]
fn clone_cow(_from: &Path, _to: &Path) -> bool {
    false
}

/// Copies one file, using a reflink when the filesystem supports it, and keeps its mtime.
fn copy_file(from: &Path, to: &Path) -> Result<()> {
    if !reflink(from, to) {
        std::fs::copy(from, to).with_context(|| format!("copying {}", from.display()))?;
    }
    if let Ok(mtime) = std::fs::metadata(from).and_then(|m| m.modified()) {
        if let Ok(f) = std::fs::File::options().write(true).open(to) {
            let _ = f.set_modified(mtime);
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn reflink(from: &Path, to: &Path) -> bool {
    use std::os::fd::AsRawFd;
    let (Ok(src), Ok(dst)) = (std::fs::File::open(from), std::fs::File::create(to)) else {
        return false;
    };
    // SAFETY: both fds are open for the duration of the call.
    let ok = unsafe { libc::ioctl(dst.as_raw_fd(), libc::FICLONE, src.as_raw_fd()) } == 0;
    if !ok {
        let _ = std::fs::remove_file(to);
    }
    ok
}

#[cfg(not(target_os = "linux"))]
fn reflink(from: &Path, to: &Path) -> bool {
    clone_cow(from, to)
}

fn copy_recursive(from: &Path, to: &Path) -> Result<()> {
    let md = std::fs::symlink_metadata(from)?;
    if md.file_type().is_symlink() {
        symlink(&std::fs::read_link(from)?, to)
    } else if md.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        copy_file(from, to)
    }
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).with_context(|| format!("linking {}", link.display()))
}

#[cfg(not(unix))]
fn symlink(target: &Path, link: &Path) -> Result<()> {
    copy_recursive(target, link)
}

/// Saves the current content of `rel` (from `src_root`) under `snap_root`, once.
pub fn snapshot_file(src_root: &Path, snap_root: &Path, rel: &str) -> Result<()> {
    let dst = snap_root.join(rel);
    let src = src_root.join(rel);
    if dst.exists() || !src.is_file() {
        return Ok(());
    }
    if let Some(p) = dst.parent() {
        std::fs::create_dir_all(p)?;
    }
    copy_file(&src, &dst)
}

pub fn remove_tree(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
}

#[derive(Debug, Clone)]
pub enum MergeOutcome {
    /// `theirs` was applied to the target (fast-forward or clean 3-way merge).
    Applied,
    /// Nothing to do: content identical to base or already equal to target.
    Unchanged,
    /// Could not merge; the target was left untouched.
    Conflict(String),
}

#[derive(Debug, Clone)]
pub struct FileMerge {
    pub path: String,
    pub outcome: MergeOutcome,
    /// Target content before the merge (text files only).
    pub before: Option<String>,
    /// Target content after the merge (text files only).
    pub after: Option<String>,
    /// Base content the sandbox started from (text files only).
    pub base: Option<String>,
    /// Sandbox content (text files only; `None` when deleted).
    pub theirs: Option<String>,
}

/// Where a merged-into target should keep pristine copies of files before
/// their first modification (so the target can itself be merged later).
pub struct SnapshotOnWrite<'a> {
    pub dir: &'a Path,
    pub manifest: &'a Manifest,
}

/// Merges every change made in `work` (since `manifest` was taken) into `target`.
///
/// Base content comes from `base_dir/<path>` when a snapshot exists, otherwise
/// from the target itself if the target file is still exactly as it was when
/// the sandbox was cloned.
pub fn merge_back(
    work: &Path,
    base_dir: &Path,
    manifest: &Manifest,
    target: &Path,
    rules: &Rules,
    snapshot: Option<SnapshotOnWrite<'_>>,
) -> Result<Vec<FileMerge>> {
    let mut seen = BTreeSet::new();
    let mut changed: Vec<String> = Vec::new();
    walk_files(work, rules, &mut |rel, abs| {
        seen.insert(rel.to_string());
        if manifest.0.get(rel) != Stamp::of(abs).as_ref() {
            changed.push(rel.to_string());
        }
    });
    changed.extend(manifest.0.keys().filter(|k| !seen.contains(*k)).cloned());

    let mut results = Vec::new();
    for rel in changed {
        let theirs = std::fs::read(work.join(&rel)).ok();
        let ours = std::fs::read(target.join(&rel)).ok();
        let base: Option<Option<Vec<u8>>> = if !manifest.0.contains_key(&rel) {
            Some(None) // added in the sandbox
        } else if let Ok(b) = std::fs::read(base_dir.join(&rel)) {
            Some(Some(b))
        } else if Stamp::of(&target.join(&rel)).as_ref() == manifest.0.get(&rel) {
            Some(ours.clone())
        } else {
            None // unknown: target changed and no snapshot was taken
        };

        let text = |b: &Option<Vec<u8>>| b.as_ref().and_then(|v| String::from_utf8(v.clone()).ok());
        let mut fm = FileMerge {
            path: rel.clone(),
            outcome: MergeOutcome::Unchanged,
            before: text(&ours),
            after: None,
            base: base.as_ref().and_then(text),
            theirs: text(&theirs),
        };

        let merged: std::result::Result<Option<Option<Vec<u8>>>, String> = match &base {
            Some(b) if *b == theirs => Ok(None),
            _ if ours == theirs => Ok(None),
            Some(b) if *b == ours => Ok(Some(theirs.clone())),
            None => Err("target changed concurrently and no base snapshot exists".into()),
            Some(None) => Err("file was created both in the sandbox and in the target".into()),
            Some(Some(b)) => match (&ours, &theirs) {
                (Some(o), Some(t)) => match (
                    std::str::from_utf8(b),
                    std::str::from_utf8(o),
                    std::str::from_utf8(t),
                ) {
                    (Ok(b), Ok(o), Ok(t)) => match diffy::merge(b, o, t) {
                        Ok(m) => Ok(Some(Some(m.into_bytes()))),
                        Err(conflict) => Err(conflict),
                    },
                    _ => Err("binary file modified on both sides".into()),
                },
                _ => Err("file deleted on one side and modified on the other".into()),
            },
        };

        match merged {
            Ok(None) => {}
            Ok(Some(new)) => {
                if let Some(snap) = &snapshot {
                    if Stamp::of(&target.join(&rel)).as_ref() == snap.manifest.0.get(&rel) {
                        snapshot_file(target, snap.dir, &rel)?;
                    }
                }
                let dst = target.join(&rel);
                match &new {
                    Some(bytes) => {
                        if let Some(p) = dst.parent() {
                            std::fs::create_dir_all(p)?;
                        }
                        std::fs::write(&dst, bytes)?;
                    }
                    None => {
                        let _ = std::fs::remove_file(&dst);
                    }
                }
                fm.after = text(&new);
                fm.outcome = MergeOutcome::Applied;
            }
            Err(detail) => fm.outcome = MergeOutcome::Conflict(detail),
        }
        if !matches!(fm.outcome, MergeOutcome::Unchanged) {
            results.push(fm);
        }
    }
    Ok(results)
}

/// The changes made in `work` since `manifest` (against `base_dir`'s snapshots), as one
/// unified diff (for review).
pub fn merge_back_preview(work: &Path, base_dir: &Path, manifest: &Manifest, rules: &Rules) -> String {
    let mut out = String::new();
    let mut seen = BTreeSet::new();
    walk_files(work, rules, &mut |rel, abs| {
        seen.insert(rel.to_string());
        if manifest.0.get(rel) != Stamp::of(abs).as_ref() {
            let old = std::fs::read_to_string(base_dir.join(rel)).unwrap_or_default();
            let new = std::fs::read_to_string(abs).unwrap_or_default();
            out.push_str(&unified_diff(rel, &old, &new));
        }
    });
    for rel in manifest.0.keys().filter(|k| !seen.contains(*k)) {
        out.push_str(&format!("--- a/{rel}\n+++ /dev/null (deleted)\n"));
    }
    if out.len() > 12_000 {
        out.truncate(12_000);
        out.push_str("\n… (diff truncated)");
    }
    out
}

/// Unified diff of `old` → `new`, labelled with `path`.
pub fn unified_diff(path: &str, old: &str, new: &str) -> String {
    let patch = diffy::create_patch(old, new).to_string();
    patch
        .replacen("--- original", &format!("--- a/{path}"), 1)
        .replacen("+++ modified", &format!("+++ b/{path}"), 1)
}

/// Whether files can be cloned copy-on-write in `dir` (tries it on a scratch file).
pub fn cow_supported(dir: &Path) -> bool {
    let probe = dir.join(format!(".cow-probe-{}", std::process::id()));
    let copy = dir.join(format!(".cow-probe-{}-copy", std::process::id()));
    let ok = std::fs::write(&probe, b"x").is_ok() && clone_cow_file(&probe, &copy);
    let _ = std::fs::remove_file(&probe);
    let _ = std::fs::remove_file(&copy);
    ok
}

