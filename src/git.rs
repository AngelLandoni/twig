//! The git twig needs, through the `git` command line: branches registered as worktrees
//! without a checkout (the files come from a copy-on-write clone), commits and merges.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().context("running git")?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The main checkout of the repository `dir` is in (also from inside a twig).
pub fn main_root(dir: &Path) -> Result<PathBuf> {
    let common = git(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).context("not inside a git repository")?;
    let common = PathBuf::from(common);
    match common.file_name() {
        Some(n) if n == ".git" => Ok(common.parent().context("no parent")?.to_path_buf()),
        _ => bail!("{} is a bare repository: twig needs a checkout", common.display()),
    }
}

/// No uncommitted changes (untracked files included; ignored ones are not).
pub fn is_clean(dir: &Path) -> bool {
    git(dir, &["status", "--porcelain"]).is_ok_and(|s| s.is_empty())
}

/// Whether `dir` is inside a git repository that has at least one commit.
pub fn has_commits(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--verify", "-q", "HEAD"]).is_ok()
}

/// The checked-out branch (None when detached or not a repository).
pub fn current_branch(dir: &Path) -> Option<String> {
    git(dir, &["symbolic-ref", "--short", "-q", "HEAD"]).ok().filter(|b| !b.is_empty())
}

pub fn branch_exists(repo: &Path, branch: &str) -> bool {
    git(repo, &["rev-parse", "--verify", "-q", &format!("refs/heads/{branch}")]).is_ok()
}

/// Whether `rev` names a commit (a branch, remote branch, tag or hash).
pub fn rev_exists(repo: &Path, rev: &str) -> bool {
    git(repo, &["rev-parse", "--verify", "-q", &format!("{rev}^{{commit}}")]).is_ok()
}

/// `name`, or `name-2`, `name-3`… if that branch exists.
pub fn unique_branch(repo: &Path, name: &str) -> String {
    let mut out = name.to_string();
    let mut n = 2;
    while branch_exists(repo, &out) {
        out = format!("{name}-{n}");
        n += 1;
    }
    out
}

pub fn delete_branch(repo: &Path, branch: &str) -> Result<()> {
    git(repo, &["branch", "-D", "-q", branch])?;
    Ok(())
}

/// Registers `dir` with `repo` on a new `branch` from `from`, without checking anything out.
pub fn attach_branch(repo: &Path, dir: &Path, branch: &str, from: &str) -> Result<()> {
    git(repo, &["worktree", "add", "--no-checkout", "-q", "-b", branch, &dir.to_string_lossy(), from])?;
    Ok(())
}

/// Forgets worktrees whose folder is gone (the branch is kept).
pub fn prune_worktrees(repo: &Path) {
    let _ = git(repo, &["worktree", "prune"]);
}

/// After files were cloned in: the index matches the branch's commit (files untouched).
pub fn sync_index(dir: &Path) -> Result<()> {
    git(dir, &["reset", "-q"])?;
    Ok(())
}

/// Makes `dir`'s tracked files exactly its HEAD commit: restores what differs and deletes
/// what the commit does not have. Ignored files (build output) are kept.
pub fn match_head(dir: &Path) -> Result<()> {
    git(dir, &["reset", "-q"])?;
    git(dir, &["checkout", "-q", "--", "."])?;
    git(dir, &["clean", "-q", "-f", "-d"])?;
    Ok(())
}

/// Writes the files of `rev` into `dest` (an empty folder).
pub fn export_tree(repo: &Path, rev: &str, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    let archive = Command::new("git").arg("-C").arg(repo).args(["archive", "--format=tar", rev]).output().context("running git archive")?;
    if !archive.status.success() {
        bail!("git archive {rev}: {}", String::from_utf8_lossy(&archive.stderr).trim());
    }
    let mut tar = Command::new("tar").arg("-x").arg("-C").arg(dest).stdin(std::process::Stdio::piped()).spawn().context("running tar")?;
    use std::io::Write;
    tar.stdin.take().context("tar stdin")?.write_all(&archive.stdout)?;
    if !tar.wait()?.success() {
        bail!("extracting {rev}");
    }
    Ok(())
}

/// `-c user.name/email` when no git identity is configured.
fn identity(dir: &Path) -> Vec<&'static str> {
    if git(dir, &["config", "user.email"]).map(|e| e.is_empty()).unwrap_or(true) {
        vec!["-c", "user.name=twig", "-c", "user.email=twig@localhost"]
    } else {
        Vec::new()
    }
}

/// Merges `rev` into `dir`'s branch. On conflicts the merge is aborted (nothing changes)
/// and the conflicting files are the error.
pub fn merge(dir: &Path, rev: &str) -> Result<std::result::Result<Vec<String>, Vec<String>>> {
    let before = git(dir, &["rev-parse", "HEAD"])?;
    let mut args = identity(dir);
    args.extend(["merge", "-q", "--no-edit", rev]);
    if git(dir, &args).is_err() {
        let conflicts: Vec<String> = git(dir, &["diff", "--name-only", "--diff-filter=U"]).map(|s| s.lines().map(str::to_string).collect()).unwrap_or_default();
        let _ = git(dir, &["merge", "--abort"]);
        return Ok(Err(if conflicts.is_empty() { vec![format!("could not merge {rev}")] } else { conflicts }));
    }
    let changed = git(dir, &["diff", "--name-only", &before, "HEAD"])?;
    Ok(Ok(changed.lines().map(str::to_string).collect()))
}

/// Each submodule's `.git` file points at the superproject's `.git/modules/…` with a
/// relative path that does not exist from a clone: makes those paths absolute.
pub fn fix_submodule_gitdirs(main: &Path, clone: &Path) {
    let Ok(staged) = git(main, &["ls-files", "--stage"]) else { return };
    for path in staged.lines().filter_map(|l| l.strip_prefix("160000 ")).filter_map(|l| l.split_once('\t')).map(|(_, p)| p) {
        let file = clone.join(path).join(".git");
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        let Some(rel) = text.trim().strip_prefix("gitdir:").map(str::trim) else { continue };
        if Path::new(rel).is_absolute() {
            continue;
        }
        if let Ok(abs) = main.join(path).join(rel).canonicalize() {
            let _ = std::fs::write(&file, format!("gitdir: {}\n", abs.display()));
        }
    }
}

/// Commits every change in `dir` (respecting .gitignore, and never the top-level dirs in
/// `skip`, in case .gitignore misses them). Returns whether a commit was made.
pub fn commit_all(dir: &Path, message: &str, skip: &[&str]) -> Result<bool> {
    // Only dirs .gitignore does not already cover (git refuses to exclude ignored paths).
    let excludes: Vec<String> = skip.iter().filter(|s| dir.join(s).exists() && git(dir, &["check-ignore", "-q", s]).is_err()).map(|s| format!(":(exclude){s}")).collect();
    let mut add = vec!["add", "-A", "--", "."];
    add.extend(excludes.iter().map(String::as_str));
    git(dir, &add)?;
    if git(dir, &["diff", "--cached", "--quiet"]).is_ok() {
        return Ok(false);
    }
    let mut args = identity(dir);
    args.extend(["commit", "-q", "--no-verify", "-m", message]);
    git(dir, &args)?;
    Ok(true)
}

/// Subjects of the commits on `branch` that `base` does not have (newest first).
pub fn commits_ahead(repo: &Path, base: &str, branch: &str) -> Vec<String> {
    git(repo, &["log", "--format=%s", &format!("{base}..{branch}")]).map(|s| s.lines().map(str::to_string).collect()).unwrap_or_default()
}
