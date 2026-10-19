//! Progress reporting for `--verbose`. Everything here goes to stderr, so
//! stdout stays clean for output meant to be piped.
//!
//! Level 1 reports each piece of work as we start it. On a terminal that is a
//! status line which the next message, or dropping its [`Step`], overwrites;
//! elsewhere it is an ordinary line, so logs keep the full record. Level 2 also
//! prints the `jj` command lines and the HTTP requests we make.

use std::io::{IsTerminal, Write};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

static LEVEL: AtomicU8 = AtomicU8::new(0);
static LINE_SHOWN: AtomicBool = AtomicBool::new(false);

/// Keeps a status line short enough that `\r` always gets back to its start.
const MAX_STATUS_WIDTH: usize = 79;

pub fn set_level(level: u8) {
    LEVEL.store(level, Ordering::Relaxed);
}

pub fn level() -> u8 {
    LEVEL.load(Ordering::Relaxed)
}

fn live() -> bool {
    std::io::stderr().is_terminal() && std::env::var_os("TERM").is_none_or(|t| t != "dumb")
}

fn erase() {
    if LINE_SHOWN.swap(false, Ordering::Relaxed) {
        eprint!("\r\x1b[K");
    }
}

/// Work in progress. Dropping it removes the status line it put up, if any.
#[must_use = "the status line disappears as soon as the step is dropped"]
pub struct Step;

impl Drop for Step {
    fn drop(&mut self) {
        erase();
    }
}

/// Say what we're about to wait for. Hold the result until the work is done.
pub fn step(msg: impl AsRef<str>) -> Step {
    let msg = msg.as_ref();
    if level() >= 2 || (level() >= 1 && !live()) {
        note(msg);
    } else if level() >= 1 {
        erase();
        let shown: String = msg.chars().take(MAX_STATUS_WIDTH).collect();
        eprint!("{shown}");
        let _ = std::io::stderr().flush();
        LINE_SHOWN.store(true, Ordering::Relaxed);
    }
    Step
}

/// A line that stays in the record. Use it when other output may follow.
pub fn note(msg: impl AsRef<str>) {
    if level() >= 1 {
        erase();
        eprintln!("{}", msg.as_ref());
    }
}

/// Detail for `-v -v`: the exact commands and requests.
pub fn trace(msg: impl AsRef<str>) {
    if level() >= 2 {
        erase();
        eprintln!("  {}", msg.as_ref());
    }
}

/// A command as you could paste it into a shell.
pub fn command_line(cmd: &Command) -> String {
    std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|a| quote(&a.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        return arg.to_string();
    }
    // Templates are laid out over several lines for us; on one line they mean the same.
    let arg = if arg.contains('\n') {
        arg.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        arg.to_string()
    };
    format!("'{}'", arg.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_only_what_the_shell_would_split() {
        assert_eq!(quote("--remote"), "--remote");
        assert_eq!(quote("trunk()..@"), "'trunk()..@'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("a\n  ++ b"), "'a ++ b'");
    }

    #[test]
    fn renders_a_command_line() {
        let mut cmd = Command::new("jj");
        cmd.args(["log", "-r", "a | b"]);
        assert_eq!(command_line(&cmd), "jj log -r 'a | b'");
    }
}
