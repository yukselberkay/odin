//! The Odin agent skill: interactive documentation that AI agents pick up on their
//! own. It uses the open Agent Skills format (a folder with `SKILL.md`), which
//! Claude Code, Codex, Gemini CLI, OpenCode, Copilot and others read.
//!
//! Odin keeps one copy in its data directory and links it into the shared
//! `~/.agents/skills` (read by Codex, Gemini CLI, OpenCode, Copilot, …) and into
//! `~/.claude/skills` and `~/.codex/skills` when those agents are set up — the same
//! layout Omarchy uses for its own skills. Windows gets copies.
use anyhow::{Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

const NAME: &str = "odin";
/// Present in every copy Odin writes, so Odin only ever replaces its own files.
const MARKER: &str = ".odin-skill";

/// Files of the skill; the references are the shipped docs, so they never drift.
const FILES: &[(&str, &str)] = &[
    ("SKILL.md", include_str!("../skills/odin/SKILL.md")),
    ("reference/manual.md", include_str!("../README.md")),
    ("reference/themes.md", include_str!("../themes/README.md")),
    (
        "reference/config.example.toml",
        include_str!("../config.example.toml"),
    ),
];

/// Where Odin keeps the skill: `~/.local/share/odin/skills/odin` on Linux,
/// `~/Library/Application Support/odin/…` on macOS, `%APPDATA%\odin\…` on Windows.
pub fn source() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
        .join("odin/skills")
        .join(NAME)
}

/// The full manual, for `:docs` and `$ODIN_DOCS`.
pub fn manual() -> PathBuf {
    source().join("reference/manual.md")
}

/// Skills folders to link into: the shared one, plus Claude Code's and Codex's own
/// folders when those agents are set up (Odin never creates config for tools you
/// don't use). Agents that read `~/.agents/skills` need nothing else.
fn targets(home: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![home.join(".agents/skills")];
    for (agent_home, skills) in [(".claude", ".claude/skills"), (".codex", ".codex/skills")] {
        if home.join(agent_home).is_dir() {
            dirs.push(home.join(skills));
        }
    }
    dirs
}

/// Folders that also read `~/.agents/skills`: an Odin link here would duplicate the
/// skill (Gemini CLI reports a conflict), so Odin removes its own links from them.
fn redundant(home: &Path) -> Vec<PathBuf> {
    [
        ".gemini/skills",
        ".copilot/skills",
        ".config/opencode/skills",
    ]
    .iter()
    .map(|d| home.join(d))
    .collect()
}

/// Remove Odin's own link or copy at `dir/odin`; returns whether there was one.
fn unlink(dir: &Path, source: &Path) -> Result<bool> {
    let link = dir.join(NAME);
    match link_state(&link, source) {
        State::Ours => fs::remove_file(&link)?,
        State::OursCopy => fs::remove_dir_all(&link)?,
        _ => return Ok(false),
    }
    Ok(true)
}

/// Write the skill files into `dir` (only what changed). Returns whether anything did.
fn write_files(dir: &Path) -> Result<bool> {
    let mut changed = false;
    for (name, content) in FILES
        .iter()
        .copied()
        .chain([(MARKER, env!("CARGO_PKG_VERSION"))])
    {
        let path = dir.join(name);
        if fs::read_to_string(&path).ok().as_deref() != Some(content) {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, content).with_context(|| format!("Write {}", path.display()))?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Make sure the skill files exist in Odin's data folder.
pub fn write_source() -> Result<PathBuf> {
    let dir = source();
    write_files(&dir)?;
    Ok(dir)
}

/// Install or update the skill for every agent; returns the folders it is visible in.
pub fn install() -> Result<Vec<PathBuf>> {
    // Tests launch agents; they must never touch the real home folders.
    if cfg!(test) {
        return Ok(Vec::new());
    }
    let source = write_source()?;
    let home = directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .context("No home directory")?;
    for dir in redundant(&home) {
        unlink(&dir, &source)?;
    }
    install_into(&source, &targets(&home))
}

fn install_into(source: &Path, targets: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut installed = Vec::new();
    for dir in targets {
        let link = dir.join(NAME);
        match link_state(&link, source) {
            State::Ours => {}
            State::OursCopy => {
                write_files(&link)?;
            }
            // Someone else's `odin` skill: leave it alone.
            State::Foreign => continue,
            State::Missing => {
                fs::create_dir_all(dir)?;
                place(source, &link)?;
            }
        }
        installed.push(link);
    }
    Ok(installed)
}

enum State {
    Ours,
    OursCopy,
    Foreign,
    Missing,
}

fn link_state(link: &Path, source: &Path) -> State {
    match fs::symlink_metadata(link) {
        Err(_) => State::Missing,
        Ok(meta) if meta.file_type().is_symlink() => match fs::read_link(link) {
            Ok(target) if target == source => State::Ours,
            // A dangling link to an older Odin location is ours to refresh.
            Ok(target) if target.ends_with("odin/skills/odin") => {
                let _ = fs::remove_file(link);
                State::Missing
            }
            _ => State::Foreign,
        },
        Ok(_) if link.join(MARKER).is_file() => State::OursCopy,
        Ok(_) => State::Foreign,
    }
}

#[cfg(unix)]
fn place(source: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(source, link).with_context(|| format!("Link {}", link.display()))
}

/// Windows symlinks need extra rights, so agents get a copy there.
#[cfg(not(unix))]
fn place(_source: &Path, link: &Path) -> Result<()> {
    write_files(link).map(|_| ())
}

/// Remove every link or copy Odin made, and its own folder.
pub fn remove() -> Result<usize> {
    if cfg!(test) {
        return Ok(0);
    }
    let source = source();
    let home = directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .context("No home directory")?;
    let mut removed = 0;
    for dir in targets(&home).into_iter().chain(redundant(&home)) {
        removed += usize::from(unlink(&dir, &source)?);
    }
    if source.join(MARKER).is_file() {
        fs::remove_dir_all(&source)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_has_agent_skills_frontmatter_and_the_manual() {
        let skill = FILES[0].1;
        assert!(skill.starts_with("---\nname: odin\ndescription:"));
        assert!(skill.contains(":font"));
        assert!(FILES
            .iter()
            .any(|(n, c)| *n == "reference/manual.md" && c.contains("Odin")));
    }

    #[test]
    fn installs_for_present_agents_and_respects_foreign_skills() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(home.join(".gemini")).unwrap();
        fs::create_dir_all(home.join(".codex/skills/odin")).unwrap(); // someone else's
        fs::write(home.join(".codex/skills/odin/SKILL.md"), "mine").unwrap();
        let source = home.join("data/odin/skills/odin");
        write_files(&source).unwrap();
        let dirs = targets(home);
        // .agents always; .claude and .codex exist; Gemini reads .agents, so no link.
        assert!(dirs.contains(&home.join(".agents/skills")));
        assert!(dirs.contains(&home.join(".claude/skills")));
        assert!(!dirs.contains(&home.join(".gemini/skills")));
        let installed = install_into(&source, &dirs).unwrap();
        assert!(installed.contains(&home.join(".claude/skills/odin")));
        assert!(!installed.contains(&home.join(".codex/skills/odin")));
        assert_eq!(
            fs::read_to_string(home.join(".codex/skills/odin/SKILL.md")).unwrap(),
            "mine"
        );
        let skill = fs::read_to_string(home.join(".agents/skills/odin/SKILL.md")).unwrap();
        assert!(skill.contains("name: odin"));
        assert!(home
            .join(".claude/skills/odin/reference/manual.md")
            .is_file());
        // Running again is a no-op that still reports the same places.
        assert_eq!(install_into(&source, &dirs).unwrap(), installed);
        assert!(!write_files(&source).unwrap());
        // A link left in a folder that also reads ~/.agents/skills is removed.
        install_into(&source, &[home.join(".gemini/skills")]).unwrap();
        assert!(unlink(&home.join(".gemini/skills"), &source).unwrap());
        assert!(!home.join(".gemini/skills/odin").exists());
        assert!(
            !unlink(&home.join(".codex/skills"), &source).unwrap(),
            "never someone else's"
        );
    }
}
