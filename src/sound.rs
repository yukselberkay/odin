//! The sound Odin plays when an AI pane finishes a prompt (config `ai_sound`).
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
};

/// What `ai_sound` asks for.
#[derive(Debug, PartialEq, Eq)]
enum Sound {
    Off,
    /// The terminal's bell (works over ssh, and when no player is installed).
    Bell,
    /// A sound file played by the system's player.
    File(PathBuf),
}

/// `"default"`, `"off"`, `"bell"`, a macOS sound name (`"Glass"`, `"Hero"`…) or a
/// path to a sound file.
fn sound(setting: &str) -> Sound {
    let setting = setting.trim();
    match setting {
        "" | "off" | "none" | "false" => Sound::Off,
        "bell" => Sound::Bell,
        "default" | "on" | "true" => default_file().map_or(Sound::Bell, Sound::File),
        name => {
            let path = match name.strip_prefix("~/") {
                Some(rest) => directories::BaseDirs::new()
                    .map_or_else(|| PathBuf::from(name), |d| d.home_dir().join(rest)),
                None => PathBuf::from(name),
            };
            let named = PathBuf::from(format!("/System/Library/Sounds/{name}.aiff"));
            if path.is_file() {
                Sound::File(path)
            } else if cfg!(target_os = "macos") && named.is_file() {
                Sound::File(named)
            } else {
                Sound::Bell
            }
        }
    }
}

/// A short "done" chime the system already has.
fn default_file() -> Option<PathBuf> {
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &["/System/Library/Sounds/Glass.aiff"]
    } else {
        &[
            "/usr/share/sounds/freedesktop/stereo/complete.oga",
            "/usr/share/sounds/freedesktop/stereo/message.oga",
        ]
    };
    candidates.iter().map(PathBuf::from).find(|p| p.is_file())
}

/// Players to try, in order, for a file.
fn players(file: &Path) -> Vec<Command> {
    let with = |program: &str, args: &[&str]| {
        let mut command = Command::new(program);
        command.args(args).arg(file);
        command
    };
    if cfg!(target_os = "macos") {
        vec![with("afplay", &[])]
    } else {
        vec![
            with("paplay", &[]),
            with("pw-play", &[]),
            with("aplay", &["-q"]),
        ]
    }
}

/// Play `ai_sound` without waiting for it; the bell when no player works.
pub fn play(setting: &str) {
    if cfg!(test) {
        return;
    }
    match sound(setting) {
        Sound::Off => {}
        Sound::Bell => bell(),
        Sound::File(file) => {
            thread::spawn(move || {
                let played = players(&file).into_iter().any(|mut player| {
                    player
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .is_ok_and(|s| s.success())
                });
                if !played {
                    bell();
                }
            });
        }
    }
}

fn bell() {
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x07").and_then(|_| out.flush());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_pick_a_sound_or_fall_back_to_the_bell() {
        assert_eq!(sound("off"), Sound::Off);
        assert_eq!(sound(""), Sound::Off);
        assert_eq!(sound("bell"), Sound::Bell);
        assert_eq!(sound("/no/such/sound.wav"), Sound::Bell);
        match default_file() {
            Some(file) => assert_eq!(sound("default"), Sound::File(file)),
            None => assert_eq!(sound("default"), Sound::Bell),
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("zing.wav");
        std::fs::write(&file, b"RIFF").unwrap();
        assert_eq!(sound(file.to_str().unwrap()), Sound::File(file));
        if cfg!(target_os = "macos") {
            assert_eq!(
                sound("Hero"),
                Sound::File("/System/Library/Sounds/Hero.aiff".into())
            );
        }
    }
}
