# twig

Work on several things at once: each twig is a parallel branch in its own folder, made in
seconds with almost no disk.

```sh
twig new login     # a parallel branch from your base, and this terminal goes into it   🌿 login
…                  # work: edit, build, git commit, run agents; other terminals stay on the base
twig status        # the twig's branch and changed files (outside a twig: every twig)
twig land          # merge it into the base branch
twig push          # push its branch to origin
twig base          # back to your normal checkout        (twig go login to come back)
twig rm            # delete it (the branch is kept unless --delete-branch)
```

Each terminal is independent. Your normal checkout stays on the base branch, and every
`twig new` in another terminal starts another parallel branch from that base. The active
twig is simply the one whose folder you are in, and your prompt shows it.

Setup, once, in `~/.zshrc` (or `~/.bashrc` with `bash`):

```sh
eval "$(twig init zsh)"
```

This lets `twig new`, `twig go`, `twig base` and `twig rm` move your terminal, and adds
`🌿 <twig>` to your prompt.

`git worktree add` also gives you another branch to work on, but it checks every file out
again and starts with an empty `target/` (or `node_modules/`), so the first build is cold.
A twig is a **copy-on-write clone** of your repository, build output included: on this
machine, a repository with a 31 GB `target/` took 2.8 s and 48 MB of disk, and it builds
warm.

## How it works

1. **A branch without a checkout.** `git worktree add --no-checkout -b twig/<name>` creates
   the branch and registers the folder. Git writes only a few KB of bookkeeping; history
   and objects stay in your repository's `.git`.
2. **A copy-on-write clone of the files.** The repository is cloned into that folder with
   `clonefile(2)` (APFS) or `FICLONE` reflinks (btrfs, xfs). The clone shares disk blocks
   with your checkout, so it costs nothing until a file changes, and then only the changed
   blocks are copied. Modification times are kept, so cargo, npm and others see the cloned
   build output as up to date and rebuild only what you change.
3. **`git reset`** makes the branch's index match the cloned files, so `git status` is clean.
4. **Your uncommitted work comes along** as the branch's first commit, so the twig's own
   commits stay clean. Or start from a branch with `--from`.
5. **Three-way merges, all or nothing.** `land` and `sync` merge file by file against the
   twig's starting point. If any file conflicts, nothing is written.

Without copy-on-write (for example ext4), build folders are shared through symlinks and
only the sources are really copied.

## Commands

| | |
|---|---|
| `twig new <name> [--from <branch>] [--branch <name>]` | a parallel branch (`twig/<name>` by default) from your checkout as it is (uncommitted work included), or from a branch or commit; goes into it |
| `twig go <name>` · `twig base` | into a twig · back to your normal checkout |
| `twig status` · `twig list` | the active twig (branch, changed files) · every twig |
| `twig land` | merges the twig into its base branch: a real `git merge` when your checkout is clean and on that branch, otherwise its changes are merged into your files (three-way, all or nothing) |
| `twig sync` | brings the base's latest into the twig |
| `twig push` | pushes its branch to origin (commits pending work first) |
| `twig rm [--delete-branch]` | deletes the twig and leaves it if you were inside; the branch is kept unless asked |
| `twig path <name>` · `twig run <name> -- <cmd>` | its folder · run a command in it |

Commands that act on a twig use the one you are in, or take its name (`twig land login`).
Inside a twig, git works as usual (it is a worktree of its branch).

Twigs live in `<repo>/.twig/<name>/`, on the same disk as the repository (copy-on-write
needs that). The folder ignores itself, so it never shows up in git or in search tools.
Each twig holds:

- `work/`: the twig's files
- `base/`: its starting point for merges
- `twig.json`: its name, branch and base

Note: `du` counts every clone at full size; `df` shows the real disk use.

## Install

```sh
cargo install --path .
```

## Library

[Blaster](../blaster) uses `twig` as a library (`twig::clone`, `twig::git`,
`twig::store::Repo`) for its operations, missions and pawns.
