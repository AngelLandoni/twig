//! Twigs: copy-on-write working copies of a repository, each on its own git branch.
//!
//! A twig lives in `<repo>/.twig/<name>/`:
//! * `work/`  — the twig: a clone of the repository (build output included) that git
//!   knows as a worktree of the twig's branch (registered with `--no-checkout`: only
//!   git's few-KB bookkeeping, the files come from the clone).
//! * `base/`  — the source as it was when the twig started (or was last synced): the
//!   common ancestor for three-way merges with the main checkout.
//! * `twig.json` — its name, branch and where it started from.
//!
//! `.twig/` ignores itself (`.gitignore` and `.ignore` with `*`), so it never shows up in
//! git or in search tools.

use crate::clone::{self, CloneMethod, MergeOutcome, Rules};
use crate::git;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const DIR: &str = ".twig";

/// Heavy, regenerable folders: cloned with the rest (copy-on-write makes them free and the
/// twig starts with a warm build), shared through symlinks when copy-on-write is missing,
/// and never part of merges.
pub const BUILD_DIRS: &[&str] = &["target", "node_modules", ".venv", "venv", "build", "dist", ".gradle", "Pods", ".next", ".turbo"];

/// Never copied: git's own folder, the twigs, and Blaster's operations.
const SKIP: &[&str] = &[".git", DIR, ".blaster"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Twig {
    pub name: String,
    pub branch: String,
    /// The branch it started from (and `sync` merges from); None: the main checkout's
    /// working copy, uncommitted work included.
    pub from: Option<String>,
    /// The branch the main checkout was on when the twig started (what `land` merges into).
    #[serde(default)]
    pub base: Option<String>,
    /// Seconds since the epoch.
    pub created: u64,
}

/// What a sync or land did: files written, or (all or nothing) the files that conflict.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub applied: Vec<String>,
    pub conflicts: Vec<(String, String)>,
    /// Landed as a real `git merge` (a commit on the base branch), not as file changes.
    pub merged_into: Option<String>,
}

/// How a twig was made.
#[derive(Debug, Clone)]
pub struct Created {
    pub twig: Twig,
    pub path: PathBuf,
    pub method: CloneMethod,
    pub cow: bool,
    pub millis: u128,
}

/// A repository's twigs.
pub struct Repo {
    pub root: PathBuf,
}

/// A twig's name: letters, digits, `-`, `_`, `.` (it is also a folder and part of a branch).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn source_rules() -> Rules {
    Rules { link: Vec::new(), exclude: SKIP.iter().chain(BUILD_DIRS).map(|s| s.to_string()).collect() }
}

impl Repo {
    /// The repository `dir` is in (from the main checkout or from inside a twig).
    pub fn discover(dir: &Path) -> Result<Repo> {
        let root = git::main_root(dir)?;
        Ok(Repo { root: root.canonicalize().unwrap_or(root) })
    }

    /// The twig a folder is in (the one "active" in a terminal there).
    pub fn current(&self, dir: &Path) -> Option<String> {
        let dir = dir.canonicalize().unwrap_or(dir.to_path_buf());
        let rest = dir.strip_prefix(self.dir()).ok()?;
        let mut parts = rest.components();
        let name = parts.next()?.as_os_str().to_string_lossy().to_string();
        (parts.next()?.as_os_str() == "work" && self.meta(&name).exists()).then_some(name)
    }

    pub fn dir(&self) -> PathBuf {
        self.root.join(DIR)
    }

    pub fn twig_dir(&self, name: &str) -> PathBuf {
        self.dir().join(name)
    }

    /// Where a twig's files are (the folder to work in).
    pub fn path(&self, name: &str) -> PathBuf {
        self.twig_dir(name).join("work")
    }

    fn base(&self, name: &str) -> PathBuf {
        self.twig_dir(name).join("base")
    }

    fn meta(&self, name: &str) -> PathBuf {
        self.twig_dir(name).join("twig.json")
    }

    /// `.twig/` exists and ignores itself.
    fn ensure_dir(&self) -> Result<()> {
        let dir = self.dir();
        std::fs::create_dir_all(&dir)?;
        for name in [".gitignore", ".ignore"] {
            let f = dir.join(name);
            if std::fs::read_to_string(&f).map(|s| s.trim() != "*").unwrap_or(true) {
                std::fs::write(&f, "*\n")?;
            }
        }
        Ok(())
    }

    pub fn get(&self, name: &str) -> Result<Twig> {
        let text = std::fs::read_to_string(self.meta(name)).with_context(|| format!("no twig named `{name}` (see `twig list`)"))?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Every twig, oldest first.
    pub fn list(&self) -> Vec<Twig> {
        let mut out: Vec<Twig> = std::fs::read_dir(self.dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| std::fs::read_to_string(e.path().join("twig.json")).ok())
            .filter_map(|t| serde_json::from_str(&t).ok())
            .collect();
        out.sort_by_key(|t: &Twig| (t.created, t.name.clone()));
        out
    }

    /// A new twig: its own branch (`branch`, else `twig/<name>`) from `from` (a branch or
    /// commit), else from the main checkout as it is now (uncommitted work included, as the
    /// branch's first commit), and a copy-on-write clone of the files.
    pub fn create(&self, name: &str, from: Option<&str>, branch: Option<&str>) -> Result<Created> {
        let started = Instant::now();
        if !valid_name(name) {
            bail!("`{name}` is not a valid twig name: use letters, digits, `-`, `_` and `.`");
        }
        if self.twig_dir(name).exists() {
            bail!("there is already a twig named `{name}`");
        }
        if !git::has_commits(&self.root) {
            bail!("the repository has no commits yet: commit something first");
        }
        if let Some(rev) = from {
            if !git::rev_exists(&self.root, rev) {
                bail!("`{rev}` is not a branch or commit of this repository");
            }
        }
        self.ensure_dir()?;
        let (dir, work, base) = (self.twig_dir(name), self.path(name), self.base(name));
        std::fs::create_dir_all(&dir)?;
        let cow = clone::cow_supported(&dir);
        let result = (|| -> Result<(Twig, CloneMethod)> {
            let branch = git::unique_branch(&self.root, branch.unwrap_or(&format!("twig/{name}")));
            git::prune_worktrees(&self.root);
            git::attach_branch(&self.root, &work, &branch, from.unwrap_or("HEAD"))?;
            // Without copy-on-write, build folders are shared instead of copied.
            let rules = Rules { link: if cow { Vec::new() } else { BUILD_DIRS.iter().map(|s| s.to_string()).collect() }, exclude: SKIP.iter().map(|s| s.to_string()).collect() };
            let method = clone::copy_tree(&self.root, &work, &rules).context("cloning the repository")?;
            git::fix_submodule_gitdirs(&self.root, &work);
            match from {
                // From the working copy: what is not committed yet becomes the branch's
                // first commit, so the twig's own commits stay clean.
                None => {
                    git::sync_index(&work)?;
                    git::commit_all(&work, "twig: uncommitted changes when the twig started", BUILD_DIRS)?;
                    clone::copy_tree(&self.root, &base, &source_rules()).context("snapshotting the base")?;
                }
                // From a branch: the files become exactly that commit (the cloned build
                // output stays, the build tool redoes what differs).
                Some(rev) => {
                    git::match_head(&work).context("checking out the base")?;
                    git::export_tree(&self.root, rev, &base).context("exporting the base")?;
                }
            }
            let created = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let base = from.map(str::to_string).or_else(|| git::current_branch(&self.root));
            let twig = Twig { name: name.into(), branch, from: from.map(str::to_string), base, created };
            std::fs::write(self.meta(name), serde_json::to_string_pretty(&twig)?)?;
            Ok((twig, method))
        })();
        match result {
            Ok((twig, method)) => Ok(Created { twig, path: work, method, cow, millis: started.elapsed().as_millis() }),
            Err(e) => {
                // Leave nothing behind: no folder, no worktree entry, no branch.
                let branch = branch.map(str::to_string).unwrap_or(format!("twig/{name}"));
                clone::remove_tree(&dir);
                git::prune_worktrees(&self.root);
                if !self.list().iter().any(|t| t.branch == branch) {
                    let _ = git::delete_branch(&self.root, &branch);
                }
                Err(e)
            }
        }
    }

    /// Source files the twig changed, added or deleted since its base.
    pub fn changed(&self, name: &str) -> Result<Vec<String>> {
        self.get(name)?;
        let (work, base) = (self.path(name), self.base(name));
        let rules = source_rules();
        let stamp = |p: &Path| std::fs::metadata(p).ok().map(|m| (m.len(), m.modified().ok()));
        let mut out = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        clone::for_each_file(&work, &rules, &mut |rel, abs| {
            seen.insert(rel.to_string());
            let b = base.join(rel);
            // Clones keep sizes and mtimes: identical stamps mean untouched.
            if stamp(abs) != stamp(&b) && std::fs::read(abs).ok() != std::fs::read(&b).ok() {
                out.push(rel.to_string());
            }
        });
        clone::for_each_file(&base, &rules, &mut |rel, _| {
            if !seen.contains(rel) {
                out.push(rel.to_string());
            }
        });
        out.sort();
        Ok(out)
    }

    /// Commits the twig's pending work on its branch.
    fn commit(&self, name: &str, message: &str) -> Result<bool> {
        git::commit_all(&self.path(name), message, BUILD_DIRS)
    }

    /// Brings the base's latest into the twig, keeping its own work (all or nothing): the
    /// main checkout's working copy when it started from it, else its base branch's new
    /// commits (a real `git merge`, aborted on conflicts).
    pub fn sync(&self, name: &str) -> Result<Report> {
        let twig = self.get(name)?;
        let (work, base) = (self.path(name), self.base(name));
        self.commit(name, "twig: work before syncing")?;
        if let Some(rev) = &twig.from {
            return Ok(match git::merge(&work, rev)? {
                Ok(applied) => {
                    clone::remove_tree(&base);
                    git::export_tree(&self.root, rev, &base)?;
                    Report { applied, conflicts: Vec::new(), merged_into: None }
                }
                Err(files) => Report { applied: Vec::new(), conflicts: files.into_iter().map(|f| (f, format!("changed on {rev} too"))).collect(), merged_into: None },
            });
        }
        let report = three_way(&self.root, &base, &work)?;
        if report.conflicts.is_empty() {
            if !report.applied.is_empty() {
                self.commit(name, "twig: sync with the main checkout")?;
            }
            self.rebase(name)?;
        }
        Ok(report)
    }

    /// Puts the twig's changes into the main checkout's files (all or nothing: on a
    /// conflict nothing is written). Its work is committed on its branch first; a twig of
    /// the working copy then starts again from the main checkout as it is now.
    pub fn land(&self, name: &str) -> Result<Report> {
        let twig = self.get(name)?;
        let (work, base) = (self.path(name), self.base(name));
        self.commit(name, "twig: work before landing")?;
        // Your checkout is clean and on the twig's base branch: a real `git merge` of the
        // twig's branch (history and all). Otherwise its changes are merged into the files.
        let on_base = twig.base.is_some() && git::current_branch(&self.root) == twig.base;
        if on_base && git::is_clean(&self.root) {
            let base_branch = twig.base.clone().unwrap_or_default();
            let report = match git::merge(&self.root, &twig.branch)? {
                Ok(applied) => Report { applied, conflicts: Vec::new(), merged_into: Some(base_branch.clone()) },
                Err(files) => Report { applied: Vec::new(), conflicts: files.into_iter().map(|f| (f, format!("changed on {base_branch} too"))).collect(), merged_into: None },
            };
            if report.conflicts.is_empty() {
                // The twig carries on from the merged base.
                let _ = git::merge(&work, &base_branch);
                self.rebase(name)?;
            }
            return Ok(report);
        }
        let report = three_way(&work, &base, &self.root)?;
        if report.conflicts.is_empty() && twig.from.is_none() {
            let catch_up = three_way(&self.root, &base, &work)?;
            if catch_up.conflicts.is_empty() {
                self.commit(name, "twig: catch up with the main checkout after landing")?;
                self.rebase(name)?;
            }
        }
        Ok(report)
    }

    /// The base becomes the main checkout as it is now.
    fn rebase(&self, name: &str) -> Result<()> {
        let base = self.base(name);
        clone::remove_tree(&base);
        clone::copy_tree(&self.root, &base, &source_rules())?;
        Ok(())
    }

    /// Pushes the twig's branch to `origin` (setting its upstream). Git asks for
    /// credentials in the terminal as usual.
    pub fn push(&self, name: &str) -> Result<String> {
        let twig = self.get(name)?;
        self.commit(name, "twig: work before pushing")?;
        let status = std::process::Command::new("git").arg("-C").arg(self.path(name)).args(["push", "-u", "origin", &twig.branch]).status()?;
        if !status.success() {
            bail!("git push failed");
        }
        Ok(twig.branch)
    }

    /// Deletes the twig's folder and git's entry for it; its branch is kept unless
    /// `delete_branch`.
    pub fn remove(&self, name: &str, delete_branch: bool) -> Result<Twig> {
        let twig = self.get(name)?;
        clone::remove_tree(&self.twig_dir(name));
        git::prune_worktrees(&self.root);
        if delete_branch {
            git::delete_branch(&self.root, &twig.branch)?;
        }
        Ok(twig)
    }
}

/// Three-way merges `work`'s changes since `base` into `target`, all or nothing: it first
/// merges into a throwaway clone of `target`, and only writes `target` when every file
/// merges cleanly.
fn three_way(work: &Path, base: &Path, target: &Path) -> Result<Report> {
    let rules = source_rules();
    let manifest = clone::manifest(base, &rules);
    let trial = base.with_file_name(format!("trial-{}", std::process::id()));
    clone::remove_tree(&trial);
    clone::copy_tree(target, &trial, &rules)?;
    let dry = clone::merge_back(work, base, &manifest, &trial, &rules, None);
    clone::remove_tree(&trial);
    let conflicts: Vec<(String, String)> = dry?
        .into_iter()
        .filter_map(|m| match m.outcome {
            MergeOutcome::Conflict(why) => Some((m.path, why)),
            _ => None,
        })
        .collect();
    if !conflicts.is_empty() {
        return Ok(Report { applied: Vec::new(), conflicts, merged_into: None });
    }
    let merged = clone::merge_back(work, base, &manifest, target, &rules, None)?;
    Ok(Report { applied: merged.into_iter().filter(|m| matches!(m.outcome, MergeOutcome::Applied)).map(|m| m.path).collect(), conflicts: Vec::new(), merged_into: None })
}
