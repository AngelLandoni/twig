//! The `twig` command on real (temporary) git repositories.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("twig-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("target/debug")).unwrap();
    std::fs::write(dir.join(".gitignore"), "/target\n").unwrap();
    std::fs::write(dir.join("src/a.rs"), "fn a() {}\n// one\n// two\n// three\nfn z() {}\n").unwrap();
    std::fs::write(dir.join("target/debug/app"), vec![7u8; 4096]).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "init"]);
    dir.canonicalize().unwrap()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=T", "-c", "user.email=t@t"]).args(args).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// Runs twig in `dir`: (success, stdout, stderr).
fn twig(dir: &Path, args: &[&str]) -> (bool, String, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_twig")).args(args).current_dir(dir).output().unwrap();
    (o.status.success(), String::from_utf8_lossy(&o.stdout).trim().to_string(), String::from_utf8_lossy(&o.stderr).trim().to_string())
}

/// Runs twig as the shell hook does: returns (success, the folder it moves the terminal to).
fn twig_cd(dir: &Path, args: &[&str]) -> (bool, Option<PathBuf>) {
    let f = std::env::temp_dir().join(format!("twig-cd-{}-{}", std::process::id(), args.join("-").replace('/', "_")));
    let _ = std::fs::remove_file(&f);
    let o = Command::new(env!("CARGO_BIN_EXE_twig")).args(args).current_dir(dir).env("TWIG_CD_FILE", &f).output().unwrap();
    let to = std::fs::read_to_string(&f).ok().map(PathBuf::from);
    let _ = std::fs::remove_file(&f);
    assert!(o.status.success() || to.is_none(), "{}", String::from_utf8_lossy(&o.stderr));
    (o.status.success(), to)
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

#[test]
fn a_twig_from_the_checkout_lands_back() {
    let main = repo("land");
    // Uncommitted work comes along.
    std::fs::write(main.join("src/b.rs"), "fn b() {}\n").unwrap();
    let (ok, to) = twig_cd(&main, &["new", "login"]);
    assert!(ok);
    let work = to.expect("twig new goes into the twig");
    assert_eq!(work, main.join(".twig/login/work"));
    assert_eq!(twig(&main, &["path", "login"]).1, work.display().to_string());
    // In the twig, it is the active one; outside, the list.
    assert!(twig(&work.join("src"), &["status"]).1.starts_with("🌿 twig login on branch twig/login"));
    assert!(twig(&main, &["status"]).1.contains("login"));
    assert!(work.join("src/b.rs").exists(), "uncommitted files are there");
    assert_eq!(read(&work.join("target/debug/app")).len(), 4096, "the build output too");
    assert_eq!(git(&work, &["branch", "--show-current"]), "twig/login");
    assert_eq!(git(&work, &["status", "--porcelain"]), "", "clean: the index matches the cloned files");
    assert_eq!(git(&main, &["log", "--format=%s", "main..twig/login"]), "twig: uncommitted changes when the twig started");
    assert!(!git(&main, &["status", "--porcelain"]).contains(".twig"), ".twig never shows in git");
    assert!(git(&main, &["status", "--porcelain", "--ignored"]).contains("!! .twig/"), "it ignores itself");

    // Work in the twig, from its own folder too.
    std::fs::write(work.join("src/a.rs"), "fn a_login() {}\n// one\n// two\n// three\nfn z() {}\n").unwrap();
    std::fs::write(work.join("src/new.rs"), "fn new() {}\n").unwrap();
    let (_, status, _) = twig(&work, &["status"]);
    assert!(status.contains("src/a.rs") && status.contains("src/new.rs"), "{status}");
    // Meanwhile the checkout changes elsewhere in the same file (uncommitted): the twig
    // lands into the files, and both changes survive.
    std::fs::write(main.join("src/a.rs"), "fn a() {}\n// one\n// two\n// three\nfn z_main() {}\n").unwrap();
    let (ok, out, err) = twig(&work, &["land"]);
    assert!(ok, "{out}{err}");
    assert_eq!(read(&main.join("src/a.rs")), "fn a_login() {}\n// one\n// two\n// three\nfn z_main() {}\n", "three-way merged");
    assert!(main.join("src/new.rs").exists());
    assert!(git(&main, &["log", "--format=%s", "main..twig/login"]).contains("twig: work before landing"), "the twig's work is a commit on its branch");
    // After landing it starts again from the checkout: nothing pending.
    assert!(twig(&main, &["status", "login"]).1.contains("no changes"));

    // Removing it from inside takes the terminal back to the checkout.
    let (ok, to) = twig_cd(&work, &["rm"]);
    assert!(ok);
    assert_eq!(to, Some(main.clone()));
    assert!(!main.join(".twig/login").exists());
    assert!(git(&main, &["branch", "--list", "twig/login"]).contains("twig/login"), "the branch stays");
    assert!(!git(&main, &["worktree", "list"]).contains(".twig/login"), "git forgot the folder");
    let _ = std::fs::remove_dir_all(&main);
}

#[test]
fn conflicts_write_nothing() {
    let main = repo("conflict");
    let work = twig_cd(&main, &["new", "x"]).1.unwrap();
    std::fs::write(work.join("src/a.rs"), "fn a_twig() {}\n// one\n// two\n// three\nfn z() {}\n").unwrap();
    std::fs::write(work.join("src/other.rs"), "fn other() {}\n").unwrap();
    std::fs::write(main.join("src/a.rs"), "fn a_main() {}\n// one\n// two\n// three\nfn z() {}\n").unwrap();
    let (ok, _, err) = twig(&main, &["land", "x"]);
    assert!(!ok && err.contains("src/a.rs"), "{err}");
    assert!(!main.join("src/other.rs").exists(), "all or nothing");
    assert_eq!(read(&main.join("src/a.rs")), "fn a_main() {}\n// one\n// two\n// three\nfn z() {}\n");
    let _ = std::fs::remove_dir_all(&main);
}

#[test]
fn a_twig_of_a_branch_syncs_with_it() {
    let main = repo("branch");
    git(&main, &["checkout", "-q", "-b", "develop"]);
    std::fs::write(main.join("src/dev.rs"), "fn dev() {}\n").unwrap();
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-q", "-m", "develop work"]);
    git(&main, &["checkout", "-q", "main"]);
    let (ok, to) = twig_cd(&main, &["new", "feat", "--from", "develop", "--branch", "angel/feat"]);
    assert!(ok);
    let work = to.unwrap();
    assert!(work.join("src/dev.rs").exists(), "develop's files");
    assert_eq!(git(&work, &["branch", "--show-current"]), "angel/feat");
    assert_eq!(git(&work, &["status", "--porcelain"]), "");
    // develop moves on: sync merges it in.
    git(&main, &["checkout", "-q", "develop"]);
    std::fs::write(main.join("src/dev2.rs"), "fn dev2() {}\n").unwrap();
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-q", "-m", "more"]);
    git(&main, &["checkout", "-q", "main"]);
    let (ok, out, _) = twig(&main, &["sync", "feat"]);
    assert!(ok && out.contains("src/dev2.rs"), "{out}");
    assert!(work.join("src/dev2.rs").exists());
    // `run` works inside it; listing shows it.
    let (ok, out, _) = twig(&main, &["run", "feat", "--", "ls", "src"]);
    assert!(ok && out.contains("dev2.rs"));
    assert!(twig(&main, &["list"]).1.contains("feat"));
    // Bad names and unknown bases are refused, leaving nothing behind.
    assert!(!twig(&main, &["new", "a b"]).0);
    assert!(!twig(&main, &["new", "y", "--from", "nope"]).0);
    assert!(!main.join(".twig/y").exists());
    let _ = std::fs::remove_dir_all(&main);
}

/// On APFS (and btrfs/xfs), a twig of a big tree is a metadata operation.
#[test]
fn big_trees_clone_instantly_on_copy_on_write() {
    let main = repo("big");
    for i in 0..20 {
        std::fs::write(main.join(format!("target/debug/blob{i}")), vec![i as u8; 1 << 20]).unwrap();
    }
    if !twig::clone::cow_supported(&main) {
        eprintln!("no copy-on-write here: skipped");
        return;
    }
    let (ok, _, err) = twig(&main, &["new", "fast"]);
    assert!(ok, "{err}");
    assert!(err.contains("copy-on-write"), "{err}");
    assert_eq!(std::fs::metadata(main.join(".twig/fast/work/target/debug/blob19")).unwrap().len(), 1 << 20);
    let _ = std::fs::remove_dir_all(&main);
}

/// A clean checkout on the twig's base: `land` is a real git merge (history kept), and
/// `push` sends the branch to origin.
#[test]
fn land_merges_the_branch_and_push_sends_it() {
    let main = repo("merge");
    let remote = main.with_extension("remote.git");
    let _ = std::fs::remove_dir_all(&remote);
    git(&main, &["init", "-q", "--bare", remote.to_str().unwrap()]);
    git(&main, &["remote", "add", "origin", remote.to_str().unwrap()]);
    let work = twig_cd(&main, &["new", "api"]).1.unwrap();
    std::fs::write(work.join("src/api.rs"), "fn api() {}\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "Add the api"]);
    std::fs::write(work.join("src/more.rs"), "fn more() {}\n").unwrap(); // not committed: twig commits it
    let (ok, out, err) = twig(&work, &["push"]);
    assert!(ok, "{err}");
    assert!(out.contains("twig/api"));
    assert!(git(&remote, &["log", "--format=%s", "twig/api"]).contains("Add the api"), "pushed");
    let (ok, out, err) = twig(&work, &["land"]);
    assert!(ok, "{err}");
    assert!(out.starts_with("Merged into main"), "{out}");
    assert!(git(&main, &["log", "--format=%s", "main"]).contains("Add the api"), "the twig's commits are on main");
    assert!(main.join("src/api.rs").exists() && main.join("src/more.rs").exists());
    assert_eq!(git(&main, &["status", "--porcelain"]), "");
    let _ = std::fs::remove_dir_all(&remote);
    let _ = std::fs::remove_dir_all(&main);
}

/// The zsh hook: `twig new` moves the terminal and the prompt shows the twig.
#[test]
fn the_zsh_hook_moves_the_terminal_and_tags_the_prompt() {
    if Command::new("zsh").arg("-c").arg("true").status().is_err() {
        return;
    }
    let main = repo("zsh");
    let bin = Path::new(env!("CARGO_BIN_EXE_twig")).parent().unwrap();
    let script = r#"eval "$(twig init zsh)"; twig new demo 2>/dev/null; _twig_prompt; print -r -- "$PWD|$_TWIG_TAG"; twig base; _twig_prompt; print -r -- "$PWD|$_TWIG_TAG""#;
    let o = Command::new("zsh").arg("-f").arg("-c").arg(script).current_dir(&main).env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())).output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "{out}{}", String::from_utf8_lossy(&o.stderr));
    assert!(lines[0].starts_with(&format!("{}|", main.join(".twig/demo/work").display())), "{out}");
    assert!(lines[0].contains("🌿 demo"), "{out}");
    assert_eq!(lines[1], format!("{}|", main.display()), "back to the base, no tag");
    let _ = std::fs::remove_dir_all(&main);
}
