//! The `twig` command: parallel branches as simple as git.

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use twig::clone::CloneMethod;
use twig::{Repo, Report};

/// Work on several things at once: each twig is a parallel branch in its own folder,
/// made in seconds with almost no disk (a copy-on-write clone, build output included).
///
/// One line in ~/.zshrc lets `twig new`, `twig go` and `twig base` move your terminal, and
/// shows the active twig in your prompt:   eval "$(twig init zsh)"
#[derive(Parser)]
#[command(name = "twig", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start a parallel branch from the base, and go into it
    New {
        name: String,
        /// Start from this branch or commit (default: your checkout as it is, uncommitted work included)
        #[arg(long)]
        from: Option<String>,
        /// Name of the branch (default: twig/<name>)
        #[arg(long)]
        branch: Option<String>,
    },
    /// Go into a twig
    #[command(alias = "switch")]
    Go { name: String },
    /// Back to your normal checkout (the base)
    #[command(aliases = ["main", "exit"])]
    Base,
    /// In a twig: its branch and changed files. Outside: every twig
    #[command(alias = "st")]
    Status { name: Option<String> },
    /// Every twig
    #[command(alias = "ls")]
    List,
    /// Merge the twig into its base branch (a real git merge when your checkout is clean)
    Land { name: Option<String> },
    /// Bring the base's latest into the twig
    Sync { name: Option<String> },
    /// Push the twig's branch to origin
    Push { name: Option<String> },
    /// Delete the twig (its branch is kept unless --delete-branch)
    #[command(alias = "remove")]
    Rm {
        name: Option<String>,
        #[arg(long)]
        delete_branch: bool,
    },
    /// Print a twig's folder
    Path { name: String },
    /// Run a command in a twig: twig run <name> -- cargo test
    Run {
        name: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// The shell hook: eval "$(twig init zsh)" in ~/.zshrc (or bash in ~/.bashrc)
    Init { shell: String },
}

const ZSH: &str = r#"# twig: `twig new/go/base/rm` move the terminal; the prompt shows the active twig.
twig() {
  local f; f=$(mktemp -t twig-cd) || return 1
  TWIG_CD_FILE=$f command twig "$@"
  local s=$?
  [ -s "$f" ] && cd "$(cat "$f")"
  rm -f "$f"
  return $s
}
_twig_prompt() {
  case "$PWD/" in
    */.twig/*/work/*) local n=${PWD#*/.twig/}; _TWIG_TAG="%F{green}🌿 ${n%%/*}%f " ;;
    *) _TWIG_TAG="" ;;
  esac
}
autoload -Uz add-zsh-hook && add-zsh-hook precmd _twig_prompt
setopt PROMPT_SUBST
case "$PROMPT" in *_TWIG_TAG*) ;; *) PROMPT='${_TWIG_TAG}'"$PROMPT" ;; esac
"#;

const BASH: &str = r#"# twig: `twig new/go/base/rm` move the terminal; the prompt shows the active twig.
twig() {
  local f; f=$(mktemp -t twig-cd.XXXXXX) || return 1
  TWIG_CD_FILE=$f command twig "$@"
  local s=$?
  [ -s "$f" ] && cd "$(cat "$f")"
  rm -f "$f"
  return $s
}
_twig_prompt() {
  case "$PWD/" in
    */.twig/*/work/*) local n=${PWD#*/.twig/}; _TWIG_TAG="🌿 ${n%%/*} " ;;
    *) _TWIG_TAG="" ;;
  esac
}
PROMPT_COMMAND="_twig_prompt${PROMPT_COMMAND:+; $PROMPT_COMMAND}"
case "$PS1" in *_TWIG_TAG*) ;; *) PS1='${_TWIG_TAG}'"$PS1" ;; esac
"#;

/// Moves the terminal there (through the shell hook), or says how.
fn go_to(dir: &Path) {
    match std::env::var_os("TWIG_CD_FILE") {
        Some(f) => {
            let _ = std::fs::write(f, dir.to_string_lossy().as_bytes());
        }
        None => eprintln!("cd \"{}\"   (eval \"$(twig init zsh)\" in ~/.zshrc does this for you)", dir.display()),
    }
}

/// The twig named, else the one this terminal is in.
fn which(repo: &Repo, cwd: &Path, name: Option<String>) -> Result<String> {
    match name.or_else(|| repo.current(cwd)) {
        Some(n) => Ok(n),
        None => bail!("not in a twig: name one, or go into it first (twig go <name>)"),
    }
}

fn report(r: &Report, done: &str) -> ExitCode {
    if !r.conflicts.is_empty() {
        eprintln!("Nothing changed: {} file(s) conflict:", r.conflicts.len());
        for (path, why) in &r.conflicts {
            eprintln!("  {path}: {why}");
        }
        return ExitCode::FAILURE;
    }
    match (&r.merged_into, r.applied.is_empty()) {
        (_, true) => println!("Nothing to {done}: already up to date."),
        (Some(base), false) => println!("Merged into {base}: {} file(s).", r.applied.len()),
        (None, false) => {
            println!("{} file(s) updated:", r.applied.len());
            for path in &r.applied {
                println!("  {path}");
            }
        }
    }
    ExitCode::SUCCESS
}

fn status(repo: &Repo, name: &str) -> Result<()> {
    let t = repo.get(name)?;
    let changed = repo.changed(name)?;
    println!("🌿 twig {} on branch {} (from {})", t.name, t.branch, t.from.as_deref().or(t.base.as_deref()).unwrap_or("your checkout"));
    if changed.is_empty() {
        println!("   no changes");
    } else {
        println!("   {} changed file(s):", changed.len());
        for f in changed {
            println!("     {f}");
        }
    }
    Ok(())
}

fn list(repo: &Repo, current: Option<&str>) {
    let twigs = repo.list();
    if twigs.is_empty() {
        println!("No twigs. Start one: twig new <name>");
    }
    for t in twigs {
        let changed = repo.changed(&t.name).map(|c| c.len()).unwrap_or(0);
        let mark = if current == Some(t.name.as_str()) { "*" } else { " " };
        println!("{mark} {:<18} {:<26} {changed} changed", t.name, t.branch);
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    if let Cmd::Init { shell } = &cli.command {
        print!("{}", if shell == "bash" { BASH } else { ZSH });
        return Ok(ExitCode::SUCCESS);
    }
    let cwd: PathBuf = std::env::current_dir()?;
    let repo = Repo::discover(&cwd)?;
    Ok(match cli.command {
        Cmd::New { name, from, branch } => {
            let c = repo.create(&name, from.as_deref(), branch.as_deref())?;
            let how = match (c.cow, c.method) {
                (true, CloneMethod::CowTree) => "copy-on-write",
                (true, CloneMethod::PerFile) => "per-file reflinks",
                (false, _) => "plain copy: no copy-on-write on this disk",
            };
            eprintln!("🌿 {} on branch {} from {} ({how}, {:.1}s)", c.twig.name, c.twig.branch, c.twig.from.as_deref().or(c.twig.base.as_deref()).unwrap_or("your checkout"), c.millis as f32 / 1000.);
            go_to(&c.path);
            ExitCode::SUCCESS
        }
        Cmd::Go { name } => {
            repo.get(&name)?;
            go_to(&repo.path(&name));
            ExitCode::SUCCESS
        }
        Cmd::Base => {
            go_to(&repo.root);
            ExitCode::SUCCESS
        }
        Cmd::Status { name } => {
            match name.or_else(|| repo.current(&cwd)) {
                Some(n) => status(&repo, &n)?,
                None => list(&repo, None),
            }
            ExitCode::SUCCESS
        }
        Cmd::List => {
            list(&repo, repo.current(&cwd).as_deref());
            ExitCode::SUCCESS
        }
        Cmd::Land { name } => report(&repo.land(&which(&repo, &cwd, name)?)?, "land"),
        Cmd::Sync { name } => report(&repo.sync(&which(&repo, &cwd, name)?)?, "sync"),
        Cmd::Push { name } => {
            let branch = repo.push(&which(&repo, &cwd, name)?)?;
            println!("Pushed {branch} to origin.");
            ExitCode::SUCCESS
        }
        Cmd::Rm { name, delete_branch } => {
            let name = which(&repo, &cwd, name)?;
            let inside = repo.current(&cwd).as_deref() == Some(name.as_str());
            let t = repo.remove(&name, delete_branch)?;
            println!("Removed {name}; {}", if delete_branch { format!("its branch {} too.", t.branch) } else { format!("its branch {} is kept.", t.branch) });
            if inside {
                go_to(&repo.root);
            }
            ExitCode::SUCCESS
        }
        Cmd::Path { name } => {
            repo.get(&name)?;
            println!("{}", repo.path(&name).display());
            ExitCode::SUCCESS
        }
        Cmd::Run { name, command } => {
            repo.get(&name)?;
            let status = std::process::Command::new(&command[0]).args(&command[1..]).current_dir(repo.path(&name)).status()?;
            ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8)
        }
        Cmd::Init { .. } => unreachable!(),
    })
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("twig: {e:#}");
            ExitCode::FAILURE
        }
    }
}
