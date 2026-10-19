use crate::config::Theme;
use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::{
    io::{Read, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
};

pub struct TerminalPane {
    pub title: String,
    pub parser: Arc<Mutex<vt100::Parser>>,
    pub dirty: Arc<AtomicBool>,
    pub exited: Option<String>,
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Box<dyn Child + Send + Sync>,
    size: (u16, u16),
    colors: Arc<Mutex<(u32, u32)>>,
}

impl TerminalPane {
    pub fn spawn(
        program: &str,
        args: &[String],
        env: &[(String, String)],
        root: &Path,
        title: &str,
        theme: &Theme,
    ) -> Result<Self> {
        let pair = native_pty_system().openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        // A full path lets Windows run `.cmd` shims (npm-installed CLIs) too.
        let mut command = CommandBuilder::new(crate::platform::resolve(program));
        command.args(args);
        command.cwd(root);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("ODIN", "1");
        for (key, value) in env {
            command.env(key, value);
        }
        // Disable inherited multiplexer identities: this is a fresh terminal.
        command.env_remove("TMUX");
        command.env_remove("STY");
        let child = pair.slave.spawn_command(command).with_context(|| {
            format!("Cannot launch {program}; install it and ensure it is on PATH")
        })?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 2_000)));
        let dirty = Arc::new(AtomicBool::new(true));
        let parser_clone = Arc::clone(&parser);
        let dirty_clone = Arc::clone(&dirty);
        let reply_writer = Arc::clone(&writer);
        let colors = Arc::new(Mutex::new(terminal_colors(theme)));
        let query_colors = Arc::clone(&colors);
        thread::spawn(move || {
            let mut bytes = [0u8; 8192];
            let mut queries = QueryResponder::default();
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        let colors = *query_colors.lock().unwrap_or_else(|e| e.into_inner());
                        let replies = if let Ok(mut parser) = parser_clone.lock() {
                            // Process in stream order so cursor reports describe the query location.
                            let mut replies = Vec::new();
                            for byte in &bytes[..count] {
                                parser.process(&[*byte]);
                                if let Some(reply) =
                                    queries.feed(*byte, parser.screen().cursor_position(), colors)
                                {
                                    replies.extend_from_slice(reply.as_bytes());
                                }
                            }
                            replies
                        } else {
                            break;
                        };
                        if !replies.is_empty() {
                            if let Ok(mut writer) = reply_writer.lock() {
                                let _ = writer.write_all(&replies);
                                let _ = writer.flush();
                            }
                        }
                        dirty_clone.store(true, Ordering::Relaxed);
                    }
                }
            }
        });
        Ok(Self {
            title: title.into(),
            parser,
            dirty,
            exited: None,
            master: pair.master,
            writer,
            child,
            size: (24, 80),
            colors,
        })
    }
    pub fn set_theme(&mut self, theme: &Theme) {
        *self.colors.lock().unwrap_or_else(|e| e.into_inner()) = terminal_colors(theme);
    }
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        let size = (rows.max(1), cols.max(1));
        if self.size != size {
            self.master.resize(PtySize {
                rows: size.0,
                cols: size.1,
                pixel_width: 0,
                pixel_height: 0,
            })?;
            if let Ok(mut parser) = self.parser.lock() {
                parser.set_size(size.0, size.1);
            }
            self.size = size;
        }
        Ok(())
    }
    pub fn send(&mut self, bytes: &[u8]) -> Result<()> {
        if self.exited.is_none() {
            let mut writer = self
                .writer
                .lock()
                .map_err(|_| anyhow::anyhow!("Terminal writer unavailable"))?;
            writer.write_all(bytes)?;
            writer.flush()?;
        }
        Ok(())
    }
    pub fn key(&mut self, key: KeyEvent) -> Result<()> {
        if let Ok(mut parser) = self.parser.lock() {
            parser.set_scrollback(0);
        }
        let app = self
            .parser
            .lock()
            .map(|p| p.screen().application_cursor())
            .unwrap_or(false);
        self.send(&encode_key(key, app))
    }
    pub fn paste(&mut self, text: &str) -> Result<()> {
        let bracketed = self
            .parser
            .lock()
            .map(|p| p.screen().bracketed_paste())
            .unwrap_or(false);
        if bracketed {
            self.send(b"\x1b[200~")?;
        }
        self.send(text.as_bytes())?;
        if bracketed {
            self.send(b"\x1b[201~")?;
        }
        Ok(())
    }
    pub fn scroll(&mut self, delta: isize) {
        if let Ok(mut parser) = self.parser.lock() {
            let scroll = parser.screen().scrollback().saturating_add_signed(delta);
            parser.set_scrollback(scroll);
        }
    }
    pub fn poll(&mut self) -> bool {
        let mut dirty = self.dirty.swap(false, Ordering::Relaxed);
        if self.exited.is_none() {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exited = Some(format!("Exited: {status}"));
                dirty = true;
            }
        }
        dirty
    }
}

fn terminal_colors(theme: &Theme) -> (u32, u32) {
    // Theme colors have already been validated at the configuration boundary.
    (
        u32::from_str_radix(&theme.foreground[1..], 16).unwrap_or(0xc9d8cf),
        u32::from_str_radix(&theme.background[1..], 16).unwrap_or(0x0b1215),
    )
}

fn color_reply(code: u8, value: u32) -> String {
    let r = ((value >> 16) & 255) * 257;
    let g = ((value >> 8) & 255) * 257;
    let b = (value & 255) * 257;
    format!("\x1b]{code};rgb:{r:04x}/{g:04x}/{b:04x}\x1b\\")
}

// Only answer bounded terminal capability queries. Output is parsed into cells;
// OSC clipboard, hyperlinks, file writes and host-side control sequences are never forwarded.
#[derive(Default)]
struct QueryResponder {
    sequence: Vec<u8>,
}
impl QueryResponder {
    fn feed(&mut self, byte: u8, cursor: (u16, u16), colors: (u32, u32)) -> Option<String> {
        if byte == 0x1b && self.sequence.last() != Some(&0x1b) {
            if self.sequence.starts_with(b"\x1b]") {
                self.sequence.push(byte);
                return None;
            }
            self.sequence.clear();
            self.sequence.push(byte);
            return None;
        }
        if self.sequence.is_empty() {
            return None;
        }
        self.sequence.push(byte);
        if self.sequence.len() > 128 {
            self.sequence.clear();
            return None;
        }
        if self.sequence.len() < 3 {
            return None;
        }
        let csi = self.sequence.get(1) == Some(&b'[');
        let osc = self.sequence.get(1) == Some(&b']');
        let complete = if csi {
            (0x40..=0x7e).contains(&byte)
        } else if osc {
            byte == 7 || self.sequence.ends_with(b"\x1b\\")
        } else {
            true
        };
        if !complete {
            return None;
        }
        let reply = match self.sequence.as_slice() {
            b"\x1b[6n" => Some(format!("\x1b[{};{}R", cursor.0 + 1, cursor.1 + 1)),
            b"\x1b[5n" => Some("\x1b[0n".into()),
            b"\x1b[c" | b"\x1b[0c" => Some("\x1b[?1;2c".into()),
            b"\x1b[>c" | b"\x1b[>0c" => Some("\x1b[>0;100;0c".into()),
            b"\x1b[?u" => Some("\x1b[?0u".into()),
            b"\x1b]10;?\x07" | b"\x1b]10;?\x1b\\" => Some(color_reply(10, colors.0)),
            b"\x1b]11;?\x07" | b"\x1b]11;?\x1b\\" => Some(color_reply(11, colors.1)),
            _ => None,
        };
        self.sequence.clear();
        reply
    }
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn encode_key(key: KeyEvent, application: bool) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let mut result = Vec::new();
    let seq = match key.code {
        KeyCode::Char(c) => {
            if alt {
                result.push(0x1b);
            }
            if ctrl && c.is_ascii() {
                result.push(match c {
                    ' ' | '@' => 0,
                    '?' => 127,
                    _ => (c.to_ascii_uppercase() as u8) & 0x1f,
                });
            } else {
                let mut bytes = [0; 4];
                result.extend_from_slice(c.encode_utf8(&mut bytes).as_bytes());
            }
            return result;
        }
        KeyCode::Enter => "\r",
        KeyCode::Tab => "\t",
        KeyCode::BackTab => "\x1b[Z",
        KeyCode::Backspace => "\x7f",
        KeyCode::Esc => "\x1b",
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Right
        | KeyCode::Left
        | KeyCode::Home
        | KeyCode::End => {
            let code = match key.code {
                KeyCode::Up => 'A',
                KeyCode::Down => 'B',
                KeyCode::Right => 'C',
                KeyCode::Left => 'D',
                KeyCode::Home => 'H',
                _ => 'F',
            };
            let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
            return if modifier > 1 {
                format!("\x1b[1;{modifier}{code}").into_bytes()
            } else {
                format!("\x1b{}{code}", if application { 'O' } else { '[' }).into_bytes()
            };
        }
        KeyCode::Delete => "\x1b[3~",
        KeyCode::Insert => "\x1b[2~",
        KeyCode::PageUp => "\x1b[5~",
        KeyCode::PageDown => "\x1b[6~",
        KeyCode::F(n) => {
            return match n {
                1..=4 => format!("\x1bO{}", (b'P' + n - 1) as char).into_bytes(),
                5..=12 => format!(
                    "\x1b[{}~",
                    [15, 17, 18, 19, 20, 21, 23, 24][(n - 5) as usize]
                )
                .into_bytes(),
                _ => Vec::new(),
            }
        }
        _ => "",
    };
    if alt {
        result.push(0x1b);
    }
    result.extend_from_slice(seq.as_bytes());
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn key_encoding_preserves_cli_controls() {
        assert_eq!(
            encode_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                false
            ),
            vec![3]
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), true),
            b"\x1bOA"
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL), false),
            b"\x1b[1;5C"
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Char('λ'), KeyModifiers::NONE), false),
            "λ".as_bytes()
        );
    }
    #[test]
    fn terminal_queries_are_bounded_and_do_not_forward_clipboard_commands() {
        let mut responder = QueryResponder::default();
        let responses: Vec<_> = b"hello\x1b[6n\x1b]52;c;secret\x07\x1b[?u"
            .iter()
            .filter_map(|byte| responder.feed(*byte, (4, 8), terminal_colors(&Theme::default())))
            .collect();
        assert_eq!(responses, vec!["\x1b[5;9R", "\x1b[?0u"]);
    }
    #[test]
    fn color_queries_follow_theme_changes() {
        let mut responder = QueryResponder::default();
        for (name, _) in crate::config::BUNDLED_THEMES {
            let theme = crate::config::load_theme_from(name, None).unwrap();
            let colors = terminal_colors(&theme);
            let replies: Vec<_> = b"\x1b]10;?\x07\x1b]11;?\x1b\\"
                .iter()
                .filter_map(|byte| responder.feed(*byte, (0, 0), colors))
                .collect();
            assert_eq!(
                replies,
                vec![color_reply(10, colors.0), color_reply(11, colors.1)]
            );
        }
        assert_eq!(
            color_reply(11, 0x1e1e1e),
            "\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\"
        );
    }
    #[cfg(unix)]
    #[test]
    fn real_pty_receives_input_and_resizes() {
        let mut terminal = TerminalPane::spawn(
            "/bin/sh",
            &[
                "-c".into(),
                "read value; printf 'received:%s' \"$value\"".into(),
            ],
            &[],
            Path::new("/tmp"),
            "test",
            &Theme::default(),
        )
        .unwrap();
        terminal.resize(12, 42).unwrap();
        terminal.send(b"hello\n").unwrap();
        for _ in 0..50 {
            if terminal
                .parser
                .lock()
                .unwrap()
                .screen()
                .contents()
                .contains("received:hello")
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("PTY did not receive shell output");
    }
}
