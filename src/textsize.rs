//! Text size (zoom in / out) for the terminal window Odin runs in. A terminal
//! program can't scale its own text, so Odin asks the terminal: Terminal.app through
//! AppleScript (only Odin's own tab changes, never the profile), xterm through its
//! font escape, kitty through remote control. Elsewhere it names the terminal's keys.
use anyhow::{bail, Context, Result};
use std::{
    io::Write,
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    In,
    Out,
    Reset,
}

impl Step {
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "in" | "+" | "bigger" => Some(Self::In),
            "out" | "-" | "smaller" => Some(Self::Out),
            "reset" | "0" => Some(Self::Reset),
            _ => None,
        }
    }
    /// Ctrl-b `+` (or `=`), `-` and `0`.
    pub fn key(c: char) -> Option<Self> {
        match c {
            '+' | '=' => Some(Self::In),
            '-' | '_' => Some(Self::Out),
            '0' => Some(Self::Reset),
            _ => None,
        }
    }
    fn word(self) -> &'static str {
        match self {
            Self::In => "bigger",
            Self::Out => "smaller",
            Self::Reset => "back to normal",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Host {
    AppleTerminal,
    Xterm,
    /// kitty with a remote-control socket (`listen_on`).
    Kitty(String),
    Other,
}

fn host(env: &dyn Fn(&str) -> Option<String>) -> Host {
    let program = env("TERM_PROGRAM").unwrap_or_default();
    if cfg!(target_os = "macos") && program == "Apple_Terminal" {
        Host::AppleTerminal
    } else if let Some(socket) = env("KITTY_LISTEN_ON").filter(|_| env("KITTY_WINDOW_ID").is_some())
    {
        Host::Kitty(socket)
    } else if env("XTERM_VERSION").is_some() && program.is_empty() {
        Host::Xterm
    } else {
        Host::Other
    }
}

/// What to tell someone whose terminal Odin can't drive.
fn keys_hint() -> String {
    let keys = if cfg!(target_os = "macos") {
        "Cmd-+ / Cmd-- / Cmd-0"
    } else {
        "Ctrl-+ / Ctrl-- / Ctrl-0 (Cmd on a Mac)"
    };
    format!("This terminal sets its own text size: {keys}")
}

/// Change the text size. The answer (a status line) arrives on the receiver, since
/// asking another program can take a moment.
pub fn change(step: Step) -> Receiver<Result<String>> {
    let (sender, receiver) = mpsc::channel();
    match host(&|name| std::env::var(name).ok()) {
        Host::Xterm => {
            // OSC 50 with `#` walks xterm's font menu; a bare `#` is its default.
            let menu = match step {
                Step::In => "#+1",
                Step::Out => "#-1",
                Step::Reset => "#",
            };
            let mut out = std::io::stdout();
            let sent = write!(out, "\x1b]50;{menu}\x07").and_then(|_| out.flush());
            let _ = sender.send(
                sent.map(|_| format!("Text {}", step.word()))
                    .map_err(Into::into),
            );
        }
        Host::AppleTerminal => {
            thread::spawn(move || {
                let _ = sender.send(apple_terminal(step));
            });
        }
        Host::Kitty(socket) => {
            thread::spawn(move || {
                let size = match step {
                    Step::In => "+1",
                    Step::Out => "-1",
                    Step::Reset => "0",
                };
                let result = run(Command::new("kitty").args([
                    "@",
                    "--to",
                    &socket,
                    "set-font-size",
                    "--",
                    size,
                ]))
                .map(|_| format!("Text {}", step.word()))
                .or_else(|_| Ok(keys_hint()));
                let _ = sender.send(result);
            });
        }
        Host::Other => {
            let _ = sender.send(Ok(keys_hint()));
        }
    }
    receiver
}

/// Resize the font of the Terminal.app tab Odin runs in (found by its tty), leaving
/// the profile and every other tab alone. Reset returns to the profile's size.
const APPLE_SCRIPT: &str = r#"on run argv
    set target to item 1 of argv
    set change to item 2 of argv
    tell application "Terminal"
        repeat with w in windows
            repeat with t in tabs of w
                if tty of t is target then
                    set s to current settings of t
                    if change is "reset" then
                        try
                            set pts to font size of settings set (name of s)
                        on error
                            set pts to font size of default settings
                        end try
                    else
                        set pts to (font size of s) + (change as integer)
                    end if
                    if pts < 6 then set pts to 6
                    if pts > 72 then set pts to 72
                    set font size of s to pts
                    return pts
                end if
            end repeat
        end repeat
    end tell
    error "Odin's Terminal tab was not found"
end run"#;

fn apple_terminal(step: Step) -> Result<String> {
    let tty = run(Command::new("ps").args(["-o", "tty=", "-p", &std::process::id().to_string()]))?;
    let tty = tty.trim();
    if tty.is_empty() || tty.contains('?') {
        bail!("Odin has no terminal tab to resize");
    }
    let change = match step {
        Step::In => "1",
        Step::Out => "-1",
        Step::Reset => "reset",
    };
    let size =
        run(Command::new("osascript").args(["-e", APPLE_SCRIPT, &format!("/dev/{tty}"), change]))
            .context("Terminal.app did not change the text size")?;
    Ok(format!(
        "Text size {} pt · Ctrl-b + / - / 0",
        size.trim().trim_end_matches(".0")
    ))
}

/// Run a helper with a short deadline; a stuck permission prompt must not hang a thread.
fn run(command: &mut Command) -> Result<String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let start = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if start.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out");
        }
        thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        bail!("{}", error.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odin_drives_the_terminals_it_can_and_names_keys_elsewhere() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        let apple = host(&env(&[("TERM_PROGRAM", "Apple_Terminal")]));
        if cfg!(target_os = "macos") {
            assert_eq!(apple, Host::AppleTerminal);
        } else {
            assert_eq!(apple, Host::Other, "AppleScript only exists on macOS");
        }
        assert_eq!(
            host(&env(&[
                ("KITTY_WINDOW_ID", "1"),
                ("KITTY_LISTEN_ON", "unix:/tmp/kitty")
            ])),
            Host::Kitty("unix:/tmp/kitty".into())
        );
        assert_eq!(host(&env(&[("KITTY_WINDOW_ID", "1")])), Host::Other);
        assert_eq!(host(&env(&[("XTERM_VERSION", "XTerm(390)")])), Host::Xterm);
        // A terminal started from xterm passes XTERM_VERSION on; trust TERM_PROGRAM.
        assert_eq!(
            host(&env(&[
                ("XTERM_VERSION", "XTerm(390)"),
                ("TERM_PROGRAM", "iTerm.app")
            ])),
            Host::Other
        );
        assert_eq!(host(&env(&[("TERM_PROGRAM", "WezTerm")])), Host::Other);
        assert!(keys_hint().contains("-0"));
        assert_eq!(Step::key('='), Some(Step::In));
        assert_eq!(Step::key('-'), Some(Step::Out));
        assert_eq!(Step::key('0'), Some(Step::Reset));
        assert_eq!(Step::parse("out"), Some(Step::Out));
        assert_eq!(Step::parse("sideways"), None);
    }
}
