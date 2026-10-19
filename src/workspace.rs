use anyhow::Result;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub depth: usize,
    pub directory: bool,
    pub symlink: bool,
}

pub struct FileTree {
    pub root: PathBuf,
    pub entries: Vec<Entry>,
    pub expanded: BTreeSet<PathBuf>,
    pub selected: usize,
    pub offset: usize,
    pub hidden: bool,
}

pub fn printable(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { '�' } else { c })
        .collect()
}

impl FileTree {
    pub fn new(root: PathBuf) -> Result<Self> {
        let mut tree = Self {
            root,
            entries: Vec::new(),
            expanded: BTreeSet::new(),
            selected: 0,
            offset: 0,
            hidden: false,
        };
        tree.refresh()?;
        Ok(tree)
    }
    pub fn refresh(&mut self) -> Result<()> {
        let selected = self.entries.get(self.selected).map(|e| e.path.clone());
        let mut entries = Vec::new();
        self.walk(&self.root, 0, &mut entries)?;
        self.entries = entries;
        if let Some(path) = selected {
            if let Some(i) = self.entries.iter().position(|e| e.path == path) {
                self.selected = i;
            }
        }
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        Ok(())
    }
    fn walk(&self, directory: &Path, depth: usize, out: &mut Vec<Entry>) -> Result<()> {
        if depth > 32 || out.len() >= 10_000 {
            return Ok(());
        }
        let mut children = fs::read_dir(directory)?
            .filter_map(|e| e.ok())
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                // Dotfiles follow the `.` toggle; .git and build output stay out of the way.
                if name == ".git"
                    || name == "target"
                    || name == "node_modules"
                    || (!self.hidden && name.starts_with('.'))
                {
                    return None;
                }
                let kind = entry.file_type().ok()?;
                Some(Entry {
                    path: entry.path(),
                    depth,
                    directory: kind.is_dir(),
                    symlink: kind.is_symlink(),
                })
            })
            .collect::<Vec<_>>();
        children.sort_by(|a, b| {
            b.directory
                .cmp(&a.directory)
                .then_with(|| a.path.file_name().cmp(&b.path.file_name()))
        });
        for entry in children {
            if out.len() >= 10_000 {
                break;
            }
            let recurse = entry.directory && self.expanded.contains(&entry.path);
            let path = entry.path.clone();
            out.push(entry);
            if recurse {
                let _ = self.walk(&path, depth + 1, out);
            }
        }
        Ok(())
    }
    pub fn down(&mut self, amount: isize) {
        self.selected = self
            .selected
            .saturating_add_signed(amount)
            .min(self.entries.len().saturating_sub(1));
    }
    pub fn activate(&mut self) -> Result<Option<PathBuf>> {
        let Some(entry) = self.entries.get(self.selected).cloned() else {
            return Ok(None);
        };
        if entry.directory {
            if !self.expanded.remove(&entry.path) {
                self.expanded.insert(entry.path);
            }
            self.refresh()?;
            Ok(None)
        } else {
            Ok(Some(entry.path))
        }
    }
    pub fn right(&mut self) -> Result<Option<PathBuf>> {
        let Some(entry) = self.entries.get(self.selected).cloned() else {
            return Ok(None);
        };
        if entry.directory {
            if self.expanded.insert(entry.path) {
                self.refresh()?;
            } else {
                self.down(1);
            }
            Ok(None)
        } else {
            Ok(Some(entry.path))
        }
    }
    pub fn left(&mut self) -> Result<()> {
        let Some(entry) = self.entries.get(self.selected).cloned() else {
            return Ok(());
        };
        if self.expanded.remove(&entry.path) {
            self.refresh()?;
        } else if let Some(parent) = entry.path.parent() {
            if let Some(i) = self.entries.iter().position(|e| e.path == parent) {
                self.selected = i;
            }
        }
        Ok(())
    }
    pub fn reveal(&mut self, path: &Path) {
        let mut parent = path.parent();
        while let Some(dir) = parent {
            if !dir.starts_with(&self.root) || dir == self.root {
                break;
            }
            self.expanded.insert(dir.to_path_buf());
            parent = dir.parent();
        }
        let _ = self.refresh();
        if let Some(i) = self.entries.iter().position(|e| e.path == path) {
            self.selected = i;
        }
    }
}

#[derive(Clone, Default)]
pub struct GitInfo {
    pub branch: String,
    pub changes: Vec<(String, String)>,
    pub graph: Vec<String>,
    pub error: Option<String>,
}

// Git runs on a worker, with no shell and no repository-provided fsmonitor.
fn git(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut child = Command::new("git")
        .arg("--no-optional-locks")
        .args(["-c", "core.fsmonitor=false", "-c", "core.pager=cat", "-C"])
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        use std::io::Read;
        let mut bytes = Vec::new();
        let _ = stdout.take(512 * 1024).read_to_end(&mut bytes);
        bytes
    });
    let start = Instant::now();
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Err(_) => break false,
            _ => {}
        }
        if start.elapsed() > Duration::from_secs(3) {
            let _ = child.kill();
            let _ = child.wait();
            break false;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let bytes = reader.join().ok()?;
    success.then_some(bytes)
}

pub fn refresh_git(root: PathBuf) -> Receiver<GitInfo> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let Some(branch) = git(&root, &["symbolic-ref", "--short", "HEAD"])
            .or_else(|| git(&root, &["rev-parse", "--short", "HEAD"]))
        else {
            let _ = sender.send(GitInfo {
                error: Some("Not a Git repository".into()),
                ..GitInfo::default()
            });
            return;
        };
        let mut info = GitInfo {
            branch: printable(String::from_utf8_lossy(&branch).trim()),
            ..GitInfo::default()
        };
        let prefix = git(&root, &["rev-parse", "--show-prefix"])
            .map(|s| {
                String::from_utf8_lossy(&s)
                    .trim_end_matches('\n')
                    .to_owned()
            })
            .unwrap_or_default();
        if let Some(status) = git(
            &root,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=normal",
                "--",
                ".",
            ],
        ) {
            let mut entries = status.split(|b| *b == 0).filter(|s| !s.is_empty());
            while let Some(entry) = entries.next() {
                if entry.len() < 3 {
                    continue;
                }
                let code = String::from_utf8_lossy(&entry[..2]).into_owned();
                let original = String::from_utf8_lossy(&entry[3..]);
                let path = original
                    .strip_prefix(&prefix)
                    .unwrap_or(&original)
                    .to_owned();
                if code.contains('R') || code.contains('C') {
                    entries.next();
                }
                info.changes.push((code, path));
            }
        } else {
            info.error = Some("Git status timed out or failed".into());
        }
        if let Some(graph) = git(
            &root,
            &[
                "log",
                "--graph",
                "--oneline",
                "--decorate=short",
                "--no-color",
                "-8",
            ],
        ) {
            info.graph = String::from_utf8_lossy(&graph)
                .lines()
                .map(printable)
                .collect();
        }
        let _ = sender.send(info);
    });
    receiver
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tree_is_sorted_lazy_and_does_not_follow_symlink_loops() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/a.rs"), "").unwrap();
        fs::write(dir.path().join("a.txt"), "").unwrap();
        std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).unwrap();
        let mut tree = FileTree::new(dir.path().into()).unwrap();
        assert_eq!(tree.entries.len(), 3);
        assert!(tree.entries[0].directory);
        tree.activate().unwrap();
        assert_eq!(tree.entries.len(), 4);
        assert!(tree.entries.iter().any(|e| e.symlink && !e.directory));
    }
}
