//! Composing commands for the remote shell.
//!
//! Everything the app runs on the roost goes through `sshd`, which hands the
//! command string to the user's shell with `-c`. Two things follow:
//!
//! - arguments that come from the UI (session names, branch names) have to be
//!   quoted, which [`quote`] does;
//! - that shell is neither a login nor an interactive shell, so it has the
//!   bare `PATH` from `sshd`, which on a Mac does not include Homebrew and so
//!   does not include `tmux`. [`in_login_shell`] re-enters the user's shell as
//!   a login shell so their profile sets `PATH` the way a terminal would.

/// Quote `s` as a single word for a POSIX shell (and for fish, whose
/// single-quote rules happen to agree with this encoding).
///
/// Plain words that contain only safe characters are returned as they are so
/// the resulting command stays readable in logs.
pub fn quote(s: &str) -> String {
    const SAFE: &[u8] = b"-_./:@%+,";
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || SAFE.contains(&b))
    {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// A shell expression for a path the user typed, expanding a leading `~` to
/// the remote `$HOME` and quoting the rest.
pub fn path_expr(path: &str) -> String {
    match path.strip_prefix("~") {
        Some("") => "\"$HOME\"".to_string(),
        Some(rest) if rest.starts_with('/') => format!("\"$HOME\"{}", quote(rest)),
        _ => quote(path),
    }
}

/// Wrap a `sh` snippet so it runs with the user's login environment.
///
/// The outer layer is interpreted by whatever shell `sshd` starts, so it is
/// kept to syntax that bash, zsh, and fish all accept. The snippet itself
/// always runs under `/bin/sh`, so callers can rely on POSIX syntax regardless
/// of the user's shell. Homebrew's directories are appended to `PATH` as a
/// fallback for profiles that only set `PATH` in their interactive rc file.
pub fn in_login_shell(snippet: &str) -> String {
    let snippet = format!("export PATH=\"$PATH:/opt/homebrew/bin:/usr/local/bin\"; {snippet}");
    let via_sh = format!("exec /bin/sh -c {}", quote(&snippet));
    format!("exec \"$SHELL\" -lc {}", quote(&via_sh))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_words_are_left_alone() {
        assert_eq!(quote("pigeons/mobile"), "pigeons/mobile");
        assert_eq!(quote("feat-x.y:z"), "feat-x.y:z");
    }

    #[test]
    fn unsafe_words_are_single_quoted() {
        assert_eq!(quote(""), "''");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote("$HOME"), "'$HOME'");
        assert_eq!(quote("=name"), "'=name'");
    }

    #[test]
    fn quoted_words_round_trip_through_sh() {
        for word in [
            "plain",
            "with space",
            "it's",
            "a\"b",
            "$(rm -rf /)",
            "`x`",
            "\\",
            "",
        ] {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("printf %s {}", quote(word)))
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), word, "{word:?}");
        }
    }

    #[test]
    fn tilde_expands_to_remote_home() {
        assert_eq!(path_expr("~"), "\"$HOME\"");
        assert_eq!(path_expr("~/code"), "\"$HOME\"/code");
        assert_eq!(path_expr("~/my code"), "\"$HOME\"'/my code'");
        assert_eq!(path_expr("/srv/code"), "/srv/code");
        assert_eq!(path_expr("~user/code"), "'~user/code'");
    }

    #[test]
    fn login_wrapper_preserves_the_snippet() {
        // Run the wrapper the way sshd would: the user's shell with -c.
        let cmd = in_login_shell(
            "printf '%s|%s' \"$1\" 'it'\\''s'; echo \" $PATH\" | grep -c /opt/homebrew/bin",
        );
        let out = std::process::Command::new("/bin/sh")
            .env("SHELL", "/bin/sh")
            .arg("-c")
            .arg(&cmd)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), "|it's1\n");
    }

    #[test]
    fn login_wrapper_works_under_zsh_and_bash() {
        for sh in ["/bin/zsh", "/bin/bash"] {
            if !std::path::Path::new(sh).exists() {
                continue;
            }
            let out = std::process::Command::new(sh)
                .env("SHELL", sh)
                .arg("-c")
                .arg(in_login_shell("echo \"$0 ok\""))
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), "/bin/sh ok\n", "{sh}");
        }
    }
}
