//! The tmux and `work.sh` vocabulary.
//!
//! `work.sh` keeps one tmux session per git worktree, named `project/tree`
//! (with `.` and `:` replaced by `-`, which tmux disallows), started in that
//! worktree's directory. Sessions are listed with `tmux list-sessions`, which
//! this module parses back into that structure so the app can group them by
//! project the way the script's menu does.
//!
//! Every function here is pure: it builds a command string or parses output.
//! Running them is the client's job, and quoting is [`crate::shell`]'s.

use serde::Serialize;

use crate::shell::{path_expr, quote};

/// Format handed to `tmux list-sessions -F`. Tab-separated so names with
/// spaces survive; tmux itself forbids tabs in session names.
pub const SESSION_FORMAT: &str = "#{session_name}\t#{session_windows}\t#{session_attached}\t#{session_created}\t#{session_activity}\t#{session_path}";

/// Terminal type requested for PTYs.
pub const TERM: &str = "xterm-256color";

/// Separates the session list from the project list in [`overview_command`]
/// output. Starts with a control character no session name or directory
/// name will.
const PROJECTS_MARKER: &str = "\u{1}pigeons:projects";

/// One tmux session, with the `project/tree` split `work.sh` encodes in its
/// name when the name has that shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Session {
    pub name: String,
    pub project: Option<String>,
    pub tree: Option<String>,
    pub windows: u32,
    pub attached: u32,
    /// Unix seconds.
    pub created: i64,
    /// Unix seconds.
    pub activity: i64,
    pub path: String,
}

/// What the session screen shows: running sessions, and the projects in the
/// code directory that new sessions can be started in.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Overview {
    pub sessions: Vec<Session>,
    pub projects: Vec<String>,
}

/// A worktree of a project, as `git worktree list --porcelain` reports it,
/// plus the tmux session name `work.sh` would give it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Worktree {
    pub name: String,
    pub path: String,
    pub branch: Option<String>,
    pub session: String,
}

/// The tmux session name `work.sh` uses for a worktree directory.
pub fn session_name(project: &str, tree: &str) -> String {
    format!("{project}/{tree}").replace(['.', ':'], "-")
}

/// Shell snippet listing sessions and projects in one round trip.
///
/// tmux failing (no server yet, or tmux missing) must not hide the projects,
/// so its exit status is ignored and the parser decides what its stderr meant.
pub fn overview_command(code_dir: &str) -> String {
    format!(
        "tmux list-sessions -F {fmt}; printf '\\001pigeons:projects\\n'; \
         for d in {code}/*/; do d=\"${{d%/}}\"; \
         if [ -e \"$d/.git\" ] || [ -d \"$d/.bare\" ]; then basename \"$d\"; fi; done",
        fmt = quote(SESSION_FORMAT),
        code = path_expr(code_dir),
    )
}

/// Whether tmux's stderr says there was simply no server, as opposed to tmux
/// being broken or missing.
pub fn is_no_server(stderr: &str) -> bool {
    let s = stderr.trim();
    s.is_empty()
        || s.contains("no server running")
        || s.contains("error connecting to")
        || s.contains("no sessions")
}

/// Parse [`overview_command`] output.
pub fn parse_overview(stdout: &str) -> Overview {
    let mut overview = Overview::default();
    let mut in_projects = false;
    for line in stdout.lines() {
        if line == PROJECTS_MARKER {
            in_projects = true;
            continue;
        }
        if in_projects {
            if !line.trim().is_empty() {
                overview.projects.push(line.trim().to_string());
            }
        } else if let Some(session) = parse_session_line(line) {
            overview.sessions.push(session);
        }
    }
    overview
}

fn parse_session_line(line: &str) -> Option<Session> {
    let mut fields = line.split('\t');
    let name = fields.next()?.to_string();
    let windows = fields.next()?.trim().parse().ok()?;
    let attached = fields.next()?.trim().parse().ok()?;
    let created = fields.next()?.trim().parse().ok()?;
    let activity = fields.next()?.trim().parse().ok()?;
    let path = fields.next().unwrap_or_default().to_string();
    let (project, tree) = match name.split_once('/') {
        Some((p, t)) if !p.is_empty() && !t.is_empty() => {
            (Some(p.to_string()), Some(t.to_string()))
        }
        _ => (None, None),
    };
    Some(Session {
        name,
        project,
        tree,
        windows,
        attached,
        created,
        activity,
        path,
    })
}

/// `git worktree list --porcelain` for a project in the code directory.
pub fn worktrees_command(code_dir: &str, project: &str) -> String {
    format!(
        "git -C {}/{} worktree list --porcelain",
        path_expr(code_dir),
        quote(project)
    )
}

/// Parse `git worktree list --porcelain`, skipping the bare repo the way
/// `work.sh` does.
pub fn parse_worktrees(stdout: &str, project: &str) -> Vec<Worktree> {
    let mut out = Vec::new();
    let mut path: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut bare = false;
    let mut flush = |path: &mut Option<String>, branch: &mut Option<String>, bare: &mut bool| {
        if let Some(p) = path.take()
            && !*bare
        {
            let name = p.rsplit('/').next().unwrap_or(&p).to_string();
            out.push(Worktree {
                session: session_name(project, &name),
                name,
                path: p,
                branch: branch.take(),
            });
        }
        *branch = None;
        *bare = false;
    };
    for line in stdout.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            flush(&mut path, &mut branch, &mut bare);
            path = Some(p.to_string());
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
        } else if line == "bare" {
            bare = true;
        } else if line.is_empty() {
            flush(&mut path, &mut branch, &mut bare);
        }
    }
    flush(&mut path, &mut branch, &mut bare);
    out
}

/// Separates origin's default branch from the branch list in
/// [`branches_command`] output.
const BRANCHES_MARKER: &str = "\u{1}pigeons:branches";

/// Branches a new worktree can start from, the way `work.sh` offers them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Branches {
    /// What `work.sh` picks when nothing is chosen: origin's `HEAD` branch,
    /// else `main` or `master` when one exists.
    pub default: Option<String>,
    /// Local and origin branches, most recently committed first, without
    /// duplicates or the `origin/` prefix.
    pub branches: Vec<String>,
}

/// Origin's default branch and every local or origin branch of a project,
/// in the order `work.sh` lists them for its "start from" prompt.
pub fn branches_command(code_dir: &str, project: &str) -> String {
    let root = format!("{}/{}", path_expr(code_dir), quote(project));
    format!(
        "git -C {root} symbolic-ref --quiet --short refs/remotes/origin/HEAD; \
         echo {marker}; \
         git -C {root} for-each-ref --sort=-committerdate --format='%(refname:short)' refs/heads refs/remotes/origin",
        marker = quote(BRANCHES_MARKER),
    )
}

/// Parse [`branches_command`] output.
pub fn parse_branches(stdout: &str) -> Branches {
    let (head, list) = stdout.split_once(BRANCHES_MARKER).unwrap_or(("", stdout));
    let mut branches: Vec<String> = Vec::new();
    for line in list.lines() {
        let b = line.trim();
        let b = b.strip_prefix("origin/").unwrap_or(b);
        if b.is_empty() || b == "origin" || b == "HEAD" || branches.iter().any(|seen| seen == b) {
            continue;
        }
        branches.push(b.to_string());
    }
    let default = head
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.strip_prefix("origin/").unwrap_or(l).to_string())
        .or_else(|| {
            ["main", "master"]
                .into_iter()
                .find(|b| branches.iter().any(|seen| seen == b))
                .map(str::to_string)
        });
    Branches { default, branches }
}

/// Attach to a session in the PTY. `exec` makes tmux the PTY's process, so
/// the channel closes exactly when the client detaches.
///
/// With `mouse`, tmux's mouse mode is switched on first. That is what lets a
/// one-finger drag on the phone scroll a pane: the terminal reports it as
/// wheel events, and tmux answers wheel-up in a normal pane by entering copy
/// mode and scrolling its history, leaving copy mode again at the bottom.
/// Without it tmux would ignore the drag entirely.
pub fn attach_command(session: &str, mouse: bool) -> String {
    let target = quote(&format!("={session}"));
    if mouse {
        format!("exec tmux set-option -g mouse on \\; attach-session -t {target}")
    } else {
        format!("exec tmux attach-session -t {target}")
    }
}

/// Kill a session by exact name.
pub fn kill_session_command(session: &str) -> String {
    format!("tmux kill-session -t {}", quote(&format!("={session}")))
}

/// Run `work.sh <project> <tree>`: opens the worktree's session, creating the
/// worktree (from a branch name) and the session when they do not exist yet.
///
/// `base` is the branch a brand-new branch starts from; it is handed over as
/// `BASE`, which the script reads instead of asking. The script ignores it
/// when the branch already exists locally or on origin, and asks in the
/// terminal when it is `None`.
pub fn work_command(work_script: &str, project: &str, tree: &str, base: Option<&str>) -> String {
    let base = match base.map(str::trim) {
        Some(b) if !b.is_empty() => format!("export BASE={}; ", quote(b)),
        _ => String::new(),
    };
    format!(
        "{base}exec {} {} {}",
        path_expr(work_script),
        quote(project),
        quote(tree)
    )
}

/// Append an `authorized_keys` line on the roost, once.
pub fn install_key_command(line: &str) -> String {
    let line = quote(line.trim());
    format!(
        "umask 077; mkdir -p \"$HOME/.ssh\"; touch \"$HOME/.ssh/authorized_keys\"; \
         grep -qxF {line} \"$HOME/.ssh/authorized_keys\" || printf '%s\\n' {line} >> \"$HOME/.ssh/authorized_keys\""
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_names_follow_work_sh() {
        assert_eq!(session_name("pigeons", "mobile"), "pigeons/mobile");
        assert_eq!(session_name("iroh", "v0.2.1"), "iroh/v0-2-1");
        assert_eq!(session_name("a.b", "c:d"), "a-b/c-d");
    }

    #[test]
    fn overview_parses_sessions_then_projects() {
        let out = "pigeons/mobile\t3\t1\t1729800000\t1729800100\t/Users/b5/code/pigeons/mobile\n\
                   scratch\t1\t0\t1729000000\t1729000001\t/Users/b5\n\
                   \u{1}pigeons:projects\n\
                   iroh\npigeons\n";
        let o = parse_overview(out);
        assert_eq!(o.projects, vec!["iroh", "pigeons"]);
        assert_eq!(o.sessions.len(), 2);
        let s = &o.sessions[0];
        assert_eq!(s.name, "pigeons/mobile");
        assert_eq!(s.project.as_deref(), Some("pigeons"));
        assert_eq!(s.tree.as_deref(), Some("mobile"));
        assert_eq!((s.windows, s.attached), (3, 1));
        assert_eq!(s.path, "/Users/b5/code/pigeons/mobile");
        let s = &o.sessions[1];
        assert_eq!(s.name, "scratch");
        assert_eq!((s.project.as_deref(), s.tree.as_deref()), (None, None));
    }

    #[test]
    fn overview_without_a_server_has_no_sessions() {
        let o = parse_overview("\u{1}pigeons:projects\npigeons\n");
        assert!(o.sessions.is_empty());
        assert_eq!(o.projects, vec!["pigeons"]);
        assert!(is_no_server(
            "no server running on /private/tmp/tmux-501/default\n"
        ));
        assert!(is_no_server(""));
        assert!(!is_no_server("sh: tmux: command not found"));
    }

    #[test]
    fn malformed_session_lines_are_skipped() {
        let o = parse_overview("garbage\nname\tx\ty\n\u{1}pigeons:projects\n");
        assert!(o.sessions.is_empty());
    }

    #[test]
    fn overview_command_runs_against_a_real_tmux_format() {
        // The command must be valid sh and print the marker even without tmux.
        let out = std::process::Command::new("/bin/sh")
            .env("PATH", "/nonexistent")
            .env("HOME", std::env::temp_dir())
            .arg("-c")
            .arg(overview_command("~/definitely-not-here"))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains(PROJECTS_MARKER), "{stdout}");
    }

    #[test]
    fn worktrees_parse_like_work_sh() {
        let porcelain = "worktree /Users/b5/code/pigeons/.bare\nbare\n\n\
                         worktree /Users/b5/code/pigeons/main\nHEAD abc\nbranch refs/heads/main\n\n\
                         worktree /Users/b5/code/pigeons/feat-x\nHEAD def\nbranch refs/heads/feat/x\n\n\
                         worktree /Users/b5/code/pigeons/detached\nHEAD 123\ndetached\n";
        let trees = parse_worktrees(porcelain, "pigeons");
        let names: Vec<_> = trees.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["main", "feat-x", "detached"]);
        assert_eq!(trees[1].branch.as_deref(), Some("feat/x"));
        assert_eq!(trees[1].session, "pigeons/feat-x");
        assert_eq!(trees[2].branch, None);
    }

    #[test]
    fn commands_quote_their_arguments() {
        assert_eq!(
            attach_command("pigeons/mobile", true),
            "exec tmux set-option -g mouse on \\; attach-session -t '=pigeons/mobile'"
        );
        assert_eq!(
            attach_command("a b", false),
            "exec tmux attach-session -t '=a b'"
        );
        assert_eq!(kill_session_command("x"), "tmux kill-session -t '=x'");
        assert_eq!(
            work_command("~/work.sh", "pigeons", "feat/x", None),
            "exec \"$HOME\"/work.sh pigeons feat/x"
        );
        assert_eq!(
            work_command("~/work.sh", "pigeons", "feat/x", Some(" ")),
            "exec \"$HOME\"/work.sh pigeons feat/x"
        );
        assert_eq!(
            work_command("~/work.sh", "pigeons", "feat/x", Some("release/1.0")),
            "export BASE=release/1.0; exec \"$HOME\"/work.sh pigeons feat/x"
        );
        assert!(branches_command("~/code", "pigeons").starts_with(
            "git -C \"$HOME\"/code/pigeons symbolic-ref --quiet --short refs/remotes/origin/HEAD; "
        ));
        assert_eq!(
            worktrees_command("~/code", "pigeons"),
            "git -C \"$HOME\"/code/pigeons worktree list --porcelain"
        );
        let install = install_key_command("ssh-ed25519 AAAA pigeons-mobile\n");
        assert!(install.contains("grep -qxF 'ssh-ed25519 AAAA pigeons-mobile'"));
    }

    #[test]
    fn branches_parse_like_work_sh() {
        let out = "origin/main\n\u{1}pigeons:branches\nfeat/x\norigin/feat/x\norigin\norigin/HEAD\norigin/main\nmain\nold\n";
        let b = parse_branches(out);
        assert_eq!(b.default.as_deref(), Some("main"));
        assert_eq!(b.branches, ["feat/x", "main", "old"]);

        // No origin/HEAD: fall back to main, then master.
        let b = parse_branches("\u{1}pigeons:branches\nfeat/x\nmaster\n");
        assert_eq!(b.default.as_deref(), Some("master"));
        let b = parse_branches("\u{1}pigeons:branches\nfeat/x\n");
        assert_eq!(b.default, None);
        assert_eq!(b.branches, ["feat/x"]);

        // The marker line runs the branch list; without it nothing is a default.
        let b = parse_branches("");
        assert_eq!(
            b,
            Branches {
                default: None,
                branches: vec![]
            }
        );
    }

    #[test]
    fn branches_command_reads_a_real_repo() {
        let code = tempfile::tempdir().unwrap();
        let git = |dir: &str, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(code.path().join(dir))
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        std::fs::create_dir_all(code.path().join("origin.git")).unwrap();
        git("origin.git", &["init", "-q", "--bare", "-b", "trunk"]);
        std::fs::create_dir_all(code.path().join("proj")).unwrap();
        git("proj", &["init", "-q", "-b", "trunk"]);
        git("proj", &["commit", "-q", "--allow-empty", "-m", "one"]);
        git("proj", &["branch", "feat/x"]);
        git(
            "proj",
            &[
                "remote",
                "add",
                "origin",
                code.path().join("origin.git").to_str().unwrap(),
            ],
        );
        git("proj", &["push", "-q", "origin", "trunk", "feat/x"]);
        git("proj", &["remote", "set-head", "origin", "-a"]);

        let cmd = branches_command(code.path().to_str().unwrap(), "proj");
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&cmd)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let b = parse_branches(&String::from_utf8_lossy(&out.stdout));
        assert_eq!(b.default.as_deref(), Some("trunk"));
        assert_eq!(b.branches.len(), 2, "{:?}", b.branches);
        assert!(b.branches.contains(&"trunk".to_string()));
        assert!(b.branches.contains(&"feat/x".to_string()));

        // Without origin/HEAD and without main/master there is no default.
        git("proj", &["remote", "set-head", "origin", "-d"]);
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&cmd)
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(
            parse_branches(&String::from_utf8_lossy(&out.stdout)).default,
            None
        );
    }

    #[test]
    fn install_key_command_is_idempotent() {
        let home = tempfile::tempdir().unwrap();
        let line = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFakeFakeFakeFakeFakeFakeFakeFakeFakeFakeFakeFak pigeons-mobile";
        for _ in 0..2 {
            let st = std::process::Command::new("/bin/sh")
                .env("HOME", home.path())
                .arg("-c")
                .arg(install_key_command(line))
                .status()
                .unwrap();
            assert!(st.success());
        }
        let content = std::fs::read_to_string(home.path().join(".ssh/authorized_keys")).unwrap();
        assert_eq!(content, format!("{line}\n"));
    }
}
