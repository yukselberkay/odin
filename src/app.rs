use crate::{
    clipboard::Clipboard,
    config::{self, Config, Theme},
    editor::Buffer,
    gitgraph::{self, GitView},
    highlight::Syntax,
    layout::{Axis, Tile},
    lsp::{self, Lsp},
    markdown::{self, Previews},
    session::{self, SavedBuffer, SavedPane, Session},
    terminal::{Launch, TerminalPane},
    workspace::{self, FileTree, GitInfo},
};
use anyhow::{bail, Context, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};

pub enum Pane {
    Editor(usize),
    Preview(Preview),
    Terminal(TerminalPane),
    Ai,
    /// Branches, commit graph and commit details (`:git`, Ctrl-b G).
    Git(Box<GitView>),
}

/// Rendered, read-only Markdown view of a buffer.
pub struct Preview {
    pub buffer: usize,
    pub scroll: usize,
    /// Source row the scroll was last aligned to; a moved cursor re-aligns it.
    pub synced: Option<usize>,
    /// Visible rows, recorded at draw time for paging.
    pub height: usize,
}

impl Preview {
    fn new(buffer: usize) -> Self {
        Self {
            buffer,
            scroll: 0,
            synced: None,
            height: 20,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    /// Character selection (`v`, or Shift+arrows).
    Visual,
    /// Whole-line selection (`V`).
    VisualLine,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Command,
    Search,
    Open,
}
pub struct Input {
    pub kind: InputKind,
    pub text: String,
}

/// Theme chooser: moving previews a theme live, Enter keeps it, Esc restores.
pub struct ThemePicker {
    /// (name, origin: "built in" / "your file" / "omarchy")
    pub themes: Vec<(String, &'static str)>,
    pub shown: Vec<usize>,
    pub selected: usize,
    pub filter: String,
    /// Theme active when the picker opened, restored on Esc.
    pub original: String,
}

/// AI CLIs the launcher offers: (key, program, name, install hint).
pub const AGENTS: [(char, &str, &str, &str); 5] = [
    ('1', "codex", "OpenAI Codex", "npm install -g @openai/codex"),
    (
        '2',
        "claude",
        "Claude Code",
        "see https://code.claude.com/docs",
    ),
    (
        '3',
        "gemini",
        "Gemini CLI",
        "npm install -g @google/gemini-cli",
    ),
    (
        '4',
        "agy",
        "Antigravity CLI",
        "curl -fsSL https://antigravity.google/cli/install.sh | bash",
    ),
    (
        '5',
        "local",
        "Local LLM (Ollama)",
        "curl -fsSL https://ollama.com/install.sh | sh",
    ),
];

/// Insert-mode completion popup.
pub struct Menu {
    pub items: Vec<lsp::Completion>,
    pub shown: Vec<usize>,
    pub selected: usize,
    pub buffer: usize,
    pub row: usize,
    /// Column where the word being completed starts.
    pub start: usize,
}

pub struct App {
    pub root: PathBuf,
    pub config: Config,
    pub theme: Theme,
    pub syntax: Syntax,
    pub previews: Previews,
    pub buffers: Vec<Buffer>,
    pub panes: BTreeMap<usize, Pane>,
    pub tiles: Tile,
    pub focused: usize,
    pub sidebar_focus: bool,
    pub sidebar_visible: bool,
    pub tree: FileTree,
    pub mode: Mode,
    pub prefix: bool,
    pub input: Option<Input>,
    pub message: String,
    pub error: bool,
    /// The F1 field guide, docked beside the panes while you keep working.
    pub help: bool,
    pub help_scroll: usize,
    pub zoom: bool,
    pub running: bool,
    pub git: GitInfo,
    pub git_focus: bool,
    pub git_selected: usize,
    pub rectangles: Vec<(usize, Rect)>,
    pub redraw: bool,
    pub count: String,
    pending: Option<char>,
    pub clipboard: Clipboard,
    pub lsp: Lsp,
    /// Hover documentation shown near the cursor until the next key.
    pub hover: Option<String>,
    pub menu: Option<Menu>,
    pub picker: Option<ThemePicker>,
    /// Buffer opened by browsing the sidebar, and its revision then. It is reused
    /// for the next browsed file unless it was opened with Enter or edited.
    peek: Option<(usize, u64)>,
    /// Screen position of the editor cursor, recorded at draw time for popups.
    pub cursor_at: Option<(u16, u16)>,
    /// Positions to return to with Ctrl-o after go-to-definition.
    jumps: Vec<(PathBuf, usize, usize)>,
    /// Mode to return to when a selection ends (Shift+arrows from insert mode).
    visual_from: Mode,
    next_pane: usize,
    git_receiver: Option<Receiver<GitInfo>>,
    last_git: Instant,
    last_reload: Instant,
    theme_stamp: Option<std::time::SystemTime>,
    completion: Option<(String, usize)>,
    /// Last session JSON written to `.odin/session.json`, to skip unchanged saves.
    session_json: String,
    last_session: Instant,
    /// Autosave this project's session (`:session forget` turns it off until restart).
    session_enabled: bool,
}

impl App {
    pub fn new(root: PathBuf, initial: Option<PathBuf>, demo: bool) -> Result<Self> {
        let config = Config::load()?;
        // A broken user theme must not stop Odin from starting.
        let (theme, theme_error) = match config::load_theme(&config.theme) {
            Ok(theme) => (theme, None),
            Err(error) => (Theme::default(), Some(format!("{error:#}"))),
        };
        let theme_stamp = config::theme_stamp(&config.theme);
        let tree = FileTree::new(root.clone())?;
        let mut panes = BTreeMap::new();
        panes.insert(0, Pane::Editor(0));
        let lsp = Lsp::new(root.clone(), config.lsp_enabled, config.lsp.clone());
        let mut app = Self {
            syntax: Syntax::new(&theme),
            previews: Previews::default(),
            root: root.clone(),
            config,
            theme,
            buffers: vec![Buffer::empty()],
            panes,
            tiles: Tile::Leaf(0),
            focused: 0,
            sidebar_focus: false,
            sidebar_visible: true,
            tree,
            mode: Mode::Normal,
            prefix: false,
            input: None,
            message: "Welcome to Odin. F1 opens the field guide.".into(),
            error: false,
            help: false,
            help_scroll: 0,
            zoom: false,
            running: true,
            git: GitInfo::default(),
            git_focus: false,
            git_selected: 0,
            rectangles: Vec::new(),
            redraw: true,
            count: String::new(),
            pending: None,
            clipboard: Clipboard::default(),
            lsp,
            hover: None,
            menu: None,
            picker: None,
            peek: None,
            cursor_at: None,
            jumps: Vec::new(),
            visual_from: Mode::Normal,
            next_pane: 1,
            git_receiver: Some(workspace::refresh_git(root)),
            last_git: Instant::now(),
            last_reload: Instant::now(),
            theme_stamp,
            completion: None,
            session_json: String::new(),
            last_session: Instant::now(),
            session_enabled: true,
        };
        if let Some(path) = initial {
            app.open(&path)?;
        }
        if demo {
            let ai = app.add_pane(Pane::Ai, Axis::Horizontal);
            if let Tile::Split { percent, .. } = &mut app.tiles {
                *percent = 63;
            }
            app.focused = ai;
            app.open_terminal(None, Some(Axis::Vertical))?;
            app.focused = 0;
            app.message = "All systems in view.  Code · shell · AI · Git".into();
        }
        if app.config.git_pane {
            let focused = app.focused;
            app.git_pane()?;
            app.focused = focused;
        }
        if let Some(error) = theme_error {
            app.fail(format!("Theme failed, using phosphor: {error}"));
        }
        Ok(app)
    }
    pub fn notice(&mut self, message: impl Into<String>) {
        self.message = message.into();
        self.error = false;
        self.redraw = true;
    }
    pub fn fail(&mut self, error: impl std::fmt::Display) {
        self.message = error.to_string();
        self.error = true;
        self.redraw = true;
    }
    pub fn buffer_id(&self) -> Option<usize> {
        if let Some(Pane::Editor(id)) = self.panes.get(&self.focused) {
            Some(*id)
        } else {
            None
        }
    }
    pub fn preview_buffer(&self) -> Option<usize> {
        if let Some(Pane::Preview(p)) = self.panes.get(&self.focused) {
            Some(p.buffer)
        } else {
            None
        }
    }
    pub fn active_buffer(&mut self) -> Option<&mut Buffer> {
        let id = self.buffer_id()?;
        self.buffers.get_mut(id)
    }
    pub fn add_pane(&mut self, pane: Pane, axis: Axis) -> usize {
        let id = self.next_pane;
        self.next_pane += 1;
        self.panes.insert(id, pane);
        self.tiles.split(self.focused, id, axis);
        self.focused = id;
        self.sidebar_focus = false;
        self.zoom = false;
        self.mode = Mode::Normal;
        id
    }
    pub fn open(&mut self, path: &Path) -> Result<()> {
        self.open_file(path, false)
    }
    /// Show the file highlighted in the sidebar without leaving the sidebar.
    fn peek_selected(&mut self) {
        let path = if self.git_focus {
            self.git
                .changes
                .get(self.git_selected)
                .map(|(_, p)| self.root.join(p))
        } else {
            self.tree
                .entries
                .get(self.tree.selected)
                .filter(|e| !e.directory)
                .map(|e| e.path.clone())
        };
        if let Some(path) = path.filter(|p| p.is_file()) {
            if let Err(error) = self.open_file(&path, true) {
                self.notice(format!("Can't preview {}: {error}", path.display()));
            }
        }
    }
    fn open_file(&mut self, path: &Path, peek: bool) -> Result<()> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        let document = |p: &Pane| matches!(p, Pane::Editor(_) | Pane::Preview(_));
        // Browsing only fills an existing editor pane; it never creates one.
        if peek && !self.panes.values().any(document) {
            return Ok(());
        }
        let existing = self
            .buffers
            .iter()
            .position(|b| b.path.as_ref() == Some(&canonical));
        let id = if let Some(id) = existing {
            id
        } else if let Some(slot) = self.peek_slot().filter(|_| peek) {
            // Replace the previous preview instead of piling up buffers.
            let mut fresh = Buffer::open(&path)?;
            fresh.revision = self.buffers[slot].revision + 1;
            self.buffers[slot] = fresh;
            self.lsp.diagnostics.remove(&slot);
            slot
        } else {
            let b = Buffer::open(&path)?;
            self.buffers.push(b);
            self.buffers.len() - 1
        };
        if peek {
            if existing.is_none() {
                self.peek = Some((id, self.buffers[id].revision));
            }
        } else if self.peek.is_some_and(|(p, _)| p == id) {
            self.peek = None;
        }
        // Existing Markdown files open rendered; new or empty ones open for writing.
        let rendered = markdown::is_markdown(&canonical) && self.buffers[id].text.len_chars() > 0;
        let pane = || {
            if rendered {
                Pane::Preview(Preview::new(id))
            } else {
                Pane::Editor(id)
            }
        };
        let document = |p: &Pane| matches!(p, Pane::Editor(_) | Pane::Preview(_));
        if self.panes.get(&self.focused).is_some_and(document) {
            self.panes.insert(self.focused, pane());
        } else if let Some((&pane_id, _)) = self.panes.iter().find(|(_, p)| document(p)) {
            self.focused = pane_id;
            self.panes.insert(pane_id, pane());
        } else {
            self.add_pane(pane(), Axis::Horizontal);
        }
        if peek {
            self.sidebar_focus = true;
            self.notice(format!(
                "Previewing {} · Enter to edit",
                self.buffers[id].name()
            ));
            return Ok(());
        }
        self.tree.reveal(&canonical);
        self.sidebar_focus = false;
        self.mode = Mode::Normal;
        self.notice(format!("Opened {}", self.buffers[id].name()));
        Ok(())
    }
    /// The preview buffer, if it may be replaced: untouched since browsing opened it
    /// and shown in at most one pane.
    fn peek_slot(&self) -> Option<usize> {
        let (id, revision) = self.peek?;
        let buffer = self.buffers.get(id)?;
        let shown = self
            .panes
            .values()
            .filter(|p| {
                matches!(p, Pane::Editor(b) | Pane::Preview(Preview { buffer: b, .. }) if *b == id)
            })
            .count();
        (buffer.revision == revision && !buffer.dirty && shown <= 1).then_some(id)
    }
    pub fn open_terminal(
        &mut self,
        program: Option<(&str, Vec<String>)>,
        split: Option<Axis>,
    ) -> Result<()> {
        self.open_terminal_with(program, &[], split)
    }
    fn open_terminal_with(
        &mut self,
        program: Option<(&str, Vec<String>)>,
        env: &[(String, String)],
        split: Option<Axis>,
    ) -> Result<()> {
        if split.is_some() && self.panes.len() >= 12 {
            bail!("Maximum of 12 panes; close one first");
        }
        let (shell, shell_args) = crate::platform::shell(self.config.shell.as_deref());
        let (program, args) = program.unwrap_or((&shell, shell_args));
        let label = Path::new(program)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(program);
        let terminal =
            TerminalPane::spawn(program, &args, env, &self.root, label, &self.theme, &[])?;
        if let Some(axis) = split {
            self.add_pane(Pane::Terminal(terminal), axis);
        } else {
            self.panes.insert(self.focused, Pane::Terminal(terminal));
        }
        self.sidebar_focus = false;
        self.mode = Mode::Normal;
        self.notice("Terminal active. Ctrl-b then an arrow changes panes; Ctrl-b b sends Ctrl-b.");
        Ok(())
    }
    fn split_editor(&mut self, axis: Axis) -> Result<()> {
        if self.panes.len() >= 12 {
            bail!("Maximum of 12 panes");
        }
        let buffer = self.buffer_id().unwrap_or_else(|| {
            self.buffers.push(Buffer::empty());
            self.buffers.len() - 1
        });
        self.add_pane(Pane::Editor(buffer), axis);
        self.notice("Split created. Ctrl-b + arrows to move; Ctrl-b z to zoom.");
        Ok(())
    }
    fn close(&mut self, force: bool) -> Result<()> {
        if let Some(id) = self.buffer_id() {
            if self.buffers[id].dirty && !force {
                bail!("Unsaved changes. :w to save, :q! to discard this view.");
            }
        }
        if self.panes.len() == 1 {
            self.quit(force)?;
            return Ok(());
        }
        let closing = self.focused;
        if self.tiles.remove(closing) {
            self.panes.remove(&closing);
        }
        self.focused = *self.panes.keys().next().context("No remaining pane")?;
        self.zoom = false;
        self.mode = Mode::Normal;
        self.notice("Pane closed");
        Ok(())
    }
    fn quit(&mut self, force: bool) -> Result<()> {
        if !force && self.buffers.iter().any(|b| b.dirty) {
            bail!("Unsaved buffers. :wa to save all, :qa! to discard and exit.");
        }
        if let Err(error) = self.save_session(false) {
            self.fail(format!("Session not saved: {error:#}"));
        }
        self.running = false;
        Ok(())
    }
    /// The workspace as it is now, for `.odin/session.json`.
    pub fn session(&self) -> Session {
        let relative = |path: &Path| {
            path.strip_prefix(&self.root)
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| path.to_path_buf())
        };
        let buffers = self
            .buffers
            .iter()
            .map(|b| {
                // Keep the text only when the file on disk won't give it back.
                let changed = b.dirty || (b.path.is_none() && b.text.len_chars() > 0);
                SavedBuffer {
                    path: b.path.as_deref().map(relative),
                    text: changed.then(|| b.text.to_string()),
                    row: b.row,
                    col: b.col,
                    scroll: b.scroll,
                }
            })
            .collect();
        let panes = self
            .panes
            .iter()
            .map(|(id, pane)| {
                let saved = match pane {
                    Pane::Editor(buffer) => SavedPane::Editor { buffer: *buffer },
                    Pane::Preview(p) => SavedPane::Preview {
                        buffer: p.buffer,
                        scroll: p.scroll,
                    },
                    Pane::Terminal(t) => {
                        let (transcript, cwd) = t.saved();
                        SavedPane::Terminal {
                            launch: t.launch.clone(),
                            cwd: cwd.map(|cwd| relative(&cwd)),
                            transcript,
                        }
                    }
                    Pane::Ai => SavedPane::Ai,
                    Pane::Git(view) => SavedPane::Git {
                        only: view.only.clone(),
                    },
                };
                (*id, saved)
            })
            .collect();
        Session::new(
            self.tiles.clone(),
            self.focused,
            self.zoom,
            self.sidebar_visible,
            buffers,
            panes,
        )
    }
    /// Save the session when it changed. `force` also starts sessions in a project
    /// that has none yet (`:session save`).
    pub fn save_session(&mut self, force: bool) -> Result<()> {
        if !self.config.restore_session || !self.session_enabled {
            return Ok(());
        }
        if !force && !session::wanted(&self.root) {
            return Ok(());
        }
        for pane in self.panes.values_mut() {
            if let Pane::Terminal(terminal) = pane {
                terminal.snapshot();
            }
        }
        let json = serde_json::to_string(&self.session())?;
        if json != self.session_json || force {
            session::save(&self.root, &json)?;
            self.session_json = json;
        }
        Ok(())
    }
    /// Reopen this project's saved session, if any. Problems are reported, never fatal.
    pub fn restore_session(&mut self) {
        if !self.config.restore_session {
            return;
        }
        match session::load(&self.root) {
            Ok(Some(saved)) => match self.restore(saved) {
                Ok(note) => self.notice(note),
                Err(error) => self.fail(format!("Session not restored: {error:#}")),
            },
            Ok(None) => {}
            Err(error) => self.fail(format!("Session not restored: {error:#}")),
        }
    }
    fn restore(&mut self, saved: Session) -> Result<String> {
        let mut buffers = Vec::new();
        let mut unsaved = 0;
        for b in saved.buffers {
            let path = b.path.map(|p| self.root.join(p));
            let mut buffer = match &path {
                Some(p) if p.is_file() => Buffer::open(p).unwrap_or_else(|_| Buffer::empty()),
                Some(p) => {
                    let mut buffer = Buffer::empty();
                    buffer.path = Some(p.clone());
                    buffer
                }
                None => Buffer::empty(),
            };
            if let Some(text) = b.text {
                if buffer.text != text.as_str() {
                    buffer.text = ropey::Rope::from_str(&text);
                    buffer.dirty = true;
                    buffer.revision += 1;
                    unsaved += 1;
                }
            }
            buffer.row = b.row;
            buffer.col = b.col;
            buffer.scroll = b.scroll;
            buffer.clamp();
            buffers.push(buffer);
        }
        if buffers.is_empty() {
            buffers.push(Buffer::empty());
        }
        let last = buffers.len() - 1;
        self.buffers = buffers;
        self.panes.clear();
        let mut problems = Vec::new();
        for (id, pane) in saved.panes {
            let pane = match pane {
                SavedPane::Editor { buffer } => Pane::Editor(buffer.min(last)),
                SavedPane::Preview { buffer, scroll } => {
                    let mut preview = Preview::new(buffer.min(last));
                    preview.scroll = scroll;
                    preview.synced = Some(self.buffers[buffer.min(last)].row);
                    Pane::Preview(preview)
                }
                SavedPane::Ai => Pane::Ai,
                SavedPane::Git { only } => {
                    let mut view = GitView::new(self.root.clone());
                    if only.is_some() {
                        view.only = only;
                        view.refresh();
                    }
                    Pane::Git(Box::new(view))
                }
                SavedPane::Terminal {
                    launch,
                    cwd,
                    transcript,
                } => match self.restore_terminal(launch, cwd, &transcript) {
                    Ok(terminal) => Pane::Terminal(terminal),
                    Err(error) => {
                        problems.push(format!("{error:#}"));
                        Pane::Ai
                    }
                },
            };
            self.panes.insert(id, pane);
        }
        self.tiles = saved.tiles;
        self.next_pane = self.panes.keys().max().map_or(0, |id| id + 1);
        self.focused = if self.panes.contains_key(&saved.focused) {
            saved.focused
        } else {
            *self.panes.keys().next().context("Session has no panes")?
        };
        self.zoom = saved.zoom;
        self.sidebar_visible = saved.sidebar_visible;
        self.sidebar_focus = false;
        self.mode = Mode::Normal;
        self.session_json = serde_json::to_string(&self.session()).unwrap_or_default();
        let mut note = format!("Session restored · {} panes", self.panes.len());
        if unsaved > 0 {
            note.push_str(&format!(" · {unsaved} unsaved buffer(s)"));
        }
        if let Some(problem) = problems.first() {
            note.push_str(&format!(" · {problem}"));
        }
        Ok(note)
    }
    /// Start a saved terminal again: shells in their last directory below their old
    /// output, AI CLIs resuming their latest conversation where they support it.
    fn restore_terminal(
        &self,
        launch: Launch,
        cwd: Option<PathBuf>,
        transcript: &str,
    ) -> Result<TerminalPane> {
        let cwd = cwd
            .map(|c| self.root.join(c))
            .filter(|c| c.is_dir())
            .unwrap_or_else(|| self.root.clone());
        let mut history = transcript.as_bytes().to_vec();
        if !history.is_empty() {
            history.extend_from_slice(
                b"\x1b[0m\r\n\x1b[2m-- restored from your last session --\x1b[0m\r\n",
            );
        }
        let agent = match &launch {
            Launch::Agent { name, model } => self.agent_command(name, model.as_deref(), true).ok(),
            _ => None,
        };
        let (program, args, env, launch, history) = match agent {
            // A resumed CLI redraws its own conversation; don't show it twice.
            Some(((program, args, env), resumed)) => {
                let history = if resumed { Vec::new() } else { history };
                (program, args, env, launch, history)
            }
            None => {
                let (shell, args) = crate::platform::shell(self.config.shell.as_deref());
                (shell, args, Vec::new(), Launch::Shell, history)
            }
        };
        let label = Path::new(&program)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&program)
            .to_string();
        let mut terminal =
            TerminalPane::spawn(&program, &args, &env, &cwd, &label, &self.theme, &history)?;
        terminal.launch = launch;
        Ok(terminal)
    }
    fn save(&mut self, path: Option<PathBuf>) -> Result<()> {
        let id = self
            .buffer_id()
            .or_else(|| self.preview_buffer())
            .context("Focus an editor pane to save")?;
        self.buffers[id].save(path)?;
        self.lsp.saved(id, &self.buffers[id]);
        self.notice(format!(
            "Saved {} · {} lines",
            self.buffers[id].name(),
            self.buffers[id].lines()
        ));
        self.tree.refresh()?;
        self.last_git = Instant::now() - Duration::from_secs(5);
        Ok(())
    }
    pub fn tick(&mut self) {
        let visible: Vec<usize> = self
            .panes
            .values()
            .filter_map(|p| match p {
                Pane::Editor(id) | Pane::Preview(Preview { buffer: id, .. }) => Some(*id),
                _ => None,
            })
            .collect();
        let mut events = self.lsp.sync(&self.buffers, &visible);
        let (replies, changed) = self.lsp.poll(&self.buffers);
        events.extend(replies);
        self.redraw |= changed || !events.is_empty();
        for event in events {
            self.lsp_event(event);
        }
        for pane in self.panes.values_mut() {
            match pane {
                Pane::Terminal(terminal) => self.redraw |= terminal.poll(),
                Pane::Git(view) => self.redraw |= view.poll(),
                _ => {}
            }
        }
        if let Some(receiver) = &self.git_receiver {
            match receiver.try_recv() {
                Ok(info) => {
                    self.git = info;
                    self.git_receiver = None;
                    self.redraw = true;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.git_receiver = None;
                }
                _ => {}
            }
        }
        if self.git_receiver.is_none() && self.last_git.elapsed() >= Duration::from_secs(4) {
            self.git_receiver = Some(workspace::refresh_git(self.root.clone()));
            self.last_git = Instant::now();
        }
        // Closing the terminal window kills Odin without a clean quit, so keep the
        // session current while you work.
        if self.last_session.elapsed() >= Duration::from_secs(5) {
            self.last_session = Instant::now();
            if let Err(error) = self.save_session(false) {
                self.session_enabled = false;
                self.fail(format!("Session autosave off: {error:#}"));
            }
        }
        if self.last_reload.elapsed() >= Duration::from_secs(2) {
            self.last_reload = Instant::now();
            // Theme files are live: saving the active one re-themes Odin immediately.
            let stamp = config::theme_stamp(&self.config.theme);
            if stamp.is_some() && stamp != self.theme_stamp {
                self.theme_stamp = stamp;
                let name = self.config.theme.clone();
                match self.apply_theme(&name) {
                    Ok(()) => self.notice(format!("Theme {name} reloaded from disk")),
                    Err(error) => self.fail(format!("Theme {name}: {error:#}")),
                }
            }
            // Refresh clean, open buffers so AI/terminal edits appear in the editor.
            for id in self
                .panes
                .values()
                .filter_map(|p| match p {
                    Pane::Editor(id) | Pane::Preview(Preview { buffer: id, .. }) => Some(*id),
                    _ => None,
                })
                .collect::<std::collections::BTreeSet<_>>()
            {
                let b = &mut self.buffers[id];
                if b.dirty {
                    continue;
                }
                if let Some(path) = b.path.clone() {
                    let stamp = std::fs::metadata(&path)
                        .ok()
                        .and_then(|m| m.modified().ok().map(|time| (time, m.len())));
                    if stamp.is_none() || stamp == b.disk_stamp {
                        continue;
                    }
                    if let Ok(mut fresh) = Buffer::open(&path) {
                        if fresh.text != b.text {
                            fresh.row = b.row;
                            fresh.col = b.col;
                            fresh.scroll = b.scroll;
                            fresh.revision = b.revision + 1;
                            fresh.clamp();
                            *b = fresh;
                            self.redraw = true;
                        } else {
                            b.disk_stamp = fresh.disk_stamp;
                        }
                    }
                }
            }
        }
    }
    pub fn event(&mut self, event: Event) {
        self.redraw = true;
        let result = match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.key(key),
            Event::Paste(text) => {
                if let Some(input) = &mut self.input {
                    input.text.extend(text.chars().filter(|c| !c.is_control()));
                    Ok(())
                } else if let Some(Pane::Terminal(terminal)) = self.panes.get_mut(&self.focused) {
                    terminal.paste(&text)
                } else if let Some(b) = self.active_buffer() {
                    // Terminal paste (Ctrl-Shift-V) inserts at the cursor in any mode.
                    b.insert(&text.replace("\r\n", "\n"));
                    b.end_group();
                    Ok(())
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            self.fail(error);
        }
        if self.mode != Mode::Insert {
            self.menu = None;
        }
    }
    fn input_start(&mut self, kind: InputKind) {
        self.input = Some(Input {
            kind,
            text: String::new(),
        });
        self.pending = None;
        self.count.clear();
    }
    fn key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // F1 toggles the guide from anywhere; everything else keeps working while it is open.
        if key.code == KeyCode::F(1) {
            self.toggle_help();
            return Ok(());
        }
        if self.help && key.modifiers.contains(KeyModifiers::ALT) {
            let step = match key.code {
                KeyCode::Up => Some(-1),
                KeyCode::Down => Some(1),
                KeyCode::PageUp => Some(-10),
                KeyCode::PageDown => Some(10),
                _ => None,
            };
            if let Some(step) = step {
                self.help_scroll = self.help_scroll.saturating_add_signed(step);
                return Ok(());
            }
        }
        if self.picker.is_some() {
            return self.picker_key(key);
        }
        if self.hover.take().is_some() && key.code == KeyCode::Esc {
            return Ok(());
        }
        if self.input.is_some() {
            return self.input_key(key);
        }
        if self.prefix {
            self.prefix = false;
            return self.prefix_key(key);
        }
        if ctrl && key.code == KeyCode::Char('b') {
            self.prefix = true;
            return Ok(());
        }
        if ctrl && key.code == KeyCode::Char('q') {
            return self.quit(false);
        }
        if ctrl && key.code == KeyCode::Char('p') {
            self.input_start(InputKind::Open);
            return Ok(());
        }
        if ctrl && key.code == KeyCode::Char('e') {
            self.sidebar_visible = true;
            self.sidebar_focus = !self.sidebar_focus;
            self.mode = Mode::Normal;
            return Ok(());
        }
        if ctrl
            && key.code == KeyCode::Char('s')
            && !matches!(self.panes.get(&self.focused), Some(Pane::Terminal(_)))
        {
            return self.save(None);
        }
        if self.sidebar_focus {
            return self.sidebar_key(key);
        }
        match self.panes.get_mut(&self.focused) {
            Some(Pane::Terminal(terminal)) => return terminal.key(key),
            Some(Pane::Preview(_)) => return self.preview_key(key),
            Some(Pane::Git(_)) => return self.git_pane_key(key),
            Some(Pane::Ai) => {
                let chosen = match key.code {
                    KeyCode::Char('c') => Some("codex"),
                    KeyCode::Char('a') => Some("claude"),
                    KeyCode::Char('g') => Some("gemini"),
                    KeyCode::Char('y') => Some("agy"),
                    KeyCode::Char('l') => Some("local"),
                    KeyCode::Char(c) => AGENTS.iter().find(|a| a.0 == c).map(|a| a.1),
                    _ => None,
                };
                if let Some(program) = chosen {
                    return self.launch_agent(program, None);
                }
                // Keys 6–9 launch your own [ai.<name>] entries, in name order.
                if let KeyCode::Char(c @ '6'..='9') = key.code {
                    let customs = self.custom_agents();
                    if let Some(name) = customs.get(c as usize - '6' as usize) {
                        return self.launch_agent(&name.clone(), None);
                    }
                }
                if key.code == KeyCode::Char(':') {
                    self.input_start(InputKind::Command);
                }
                return Ok(());
            }
            _ => {}
        }
        if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
            return self.visual_key(key);
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if shift && is_motion(key.code) && self.buffer_id().is_some() {
            self.start_visual(Mode::Visual);
            return self.visual_key(key);
        }
        if ctrl && self.buffer_id().is_some() {
            match key.code {
                KeyCode::Char('c') => return self.copy_line(false),
                KeyCode::Char('x') => return self.copy_line(true),
                KeyCode::Char('v') => return self.paste(true),
                _ => {}
            }
        }
        if self.mode == Mode::Insert {
            return self.insert_key(key);
        }
        self.normal_key(key)
    }
    fn start_visual(&mut self, mode: Mode) {
        let from = if self.mode == Mode::Insert {
            Mode::Insert
        } else {
            Mode::Normal
        };
        if let Some(b) = self.active_buffer() {
            b.end_group();
            b.anchor = Some(b.index());
            self.visual_from = from;
            self.mode = mode;
        }
    }
    fn end_visual(&mut self) {
        if let Some(b) = self.active_buffer() {
            b.anchor = None;
        }
        self.mode = self.visual_from;
    }
    /// The selected char range, if a selection is active in the focused editor.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let b = &self.buffers[self.buffer_id()?];
        match self.mode {
            Mode::Visual => b.selection(self.visual_from != Mode::Insert, false),
            Mode::VisualLine => b.selection(true, true),
            _ => None,
        }
    }
    /// Copy (or cut) the selection; returns false when nothing is selected.
    fn copy_selection(&mut self, cut: bool) -> bool {
        let Some((start, end)) = self.selection() else {
            return false;
        };
        let linewise = self.mode == Mode::VisualLine;
        let id = self.buffer_id().expect("selection implies editor");
        let text = self.buffers[id].slice(start, end);
        let lines = text.matches('\n').count().max(1);
        self.clipboard.copy(text, linewise);
        let b = &mut self.buffers[id];
        if cut {
            b.replace(start, end, "");
        } else {
            let anchor = start;
            b.anchor = None;
            b.row = b.text.char_to_line(anchor);
            b.col = anchor - b.text.line_to_char(b.row);
        }
        self.end_visual();
        let what = if linewise {
            format!("{lines} line{}", if lines == 1 { "" } else { "s" })
        } else {
            "selection".into()
        };
        self.notice(format!(
            "{} {what} · Ctrl-v or p pastes",
            if cut { "Cut" } else { "Copied" }
        ));
        true
    }
    /// Ctrl-c / Ctrl-x without a selection work on the whole cursor line.
    fn copy_line(&mut self, cut: bool) -> Result<()> {
        let id = self.buffer_id().context("Focus an editor")?;
        let b = &mut self.buffers[id];
        let text = if cut {
            b.delete_line()
        } else {
            let start = b.char_index(b.row, 0);
            let end = b.char_index(b.row + 1, 0).max(start + b.line_len(b.row));
            let line = b.slice(start, end);
            if line.ends_with('\n') {
                line
            } else {
                format!("{line}\n")
            }
        };
        self.clipboard.copy(text, true);
        self.notice(if cut {
            "Cut line · Ctrl-v pastes it above the cursor line"
        } else {
            "Copied line · Ctrl-v pastes it above the cursor line, p below"
        });
        Ok(())
    }
    /// Paste the clipboard. Whole lines go above (Ctrl-v, `P`) or below (`p`) the
    /// cursor line; other text goes at the cursor (Ctrl-v, `P`) or after it (`p`).
    fn paste(&mut self, before: bool) -> Result<()> {
        let (text, linewise) = self.clipboard.paste();
        if text.is_empty() {
            self.notice("Clipboard is empty");
            return Ok(());
        }
        let insert = self.mode == Mode::Insert;
        let b = self.active_buffer().context("Focus an editor")?;
        b.end_group();
        if linewise {
            b.paste_lines(&text, before);
        } else {
            if !before && !insert && b.line_len(b.row) > 0 {
                b.move_by(1, 0);
            }
            let index = b.index();
            b.replace(index, index, &text);
        }
        Ok(())
    }
    fn visual_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let from_insert = self.visual_from == Mode::Insert;
        match key.code {
            KeyCode::Esc => self.end_visual(),
            KeyCode::Char('c') if ctrl => {
                self.copy_selection(false);
            }
            KeyCode::Char('x') if ctrl => {
                self.copy_selection(true);
            }
            KeyCode::Char('v') if ctrl => self.replace_selection_with_clipboard()?,
            KeyCode::Char('d' | 'u') if ctrl => self.normal_key(key)?,
            _ if ctrl && !is_motion(key.code) => {}
            KeyCode::Backspace | KeyCode::Delete => self.delete_selection(),
            // Plain arrows end a Shift-selection, like any text field.
            code if from_insert && !shift && is_motion(code) => {
                self.end_visual();
                return self.insert_key(key);
            }
            code if is_motion(code) || (!from_insert && is_vim_motion(code)) => {
                let key = KeyEvent::new(code, key.modifiers - KeyModifiers::SHIFT);
                self.normal_key(key)?;
            }
            // Typing over a Shift-selection replaces it.
            KeyCode::Char(c) if from_insert => {
                self.delete_selection();
                if let Some(b) = self.active_buffer() {
                    b.insert(&c.to_string());
                }
            }
            KeyCode::Enter if from_insert => {
                self.delete_selection();
                if let Some(b) = self.active_buffer() {
                    b.newline();
                }
            }
            KeyCode::Char('v') => {
                if self.mode == Mode::Visual {
                    self.end_visual();
                } else {
                    self.mode = Mode::Visual;
                }
            }
            KeyCode::Char('V') => {
                if self.mode == Mode::VisualLine {
                    self.end_visual();
                } else {
                    self.mode = Mode::VisualLine;
                }
            }
            KeyCode::Char('y') => {
                self.copy_selection(false);
            }
            KeyCode::Char('d') | KeyCode::Char('x') => {
                self.copy_selection(true);
            }
            KeyCode::Char('c') | KeyCode::Char('s') => {
                self.copy_selection(true);
                self.mode = Mode::Insert;
            }
            KeyCode::Char('p') | KeyCode::Char('P') => self.replace_selection_with_clipboard()?,
            _ => {}
        }
        Ok(())
    }
    fn delete_selection(&mut self) {
        if let Some((start, end)) = self.selection() {
            if let Some(b) = self.active_buffer() {
                b.replace(start, end, "");
            }
        }
        self.end_visual();
    }
    fn replace_selection_with_clipboard(&mut self) -> Result<()> {
        let (text, _) = self.clipboard.paste();
        if let Some((start, end)) = self.selection() {
            let b = self.active_buffer().context("Focus an editor")?;
            b.replace(start, end, &text);
        }
        self.end_visual();
        Ok(())
    }
    fn prefix_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('%') => self.split_editor(Axis::Horizontal)?,
            KeyCode::Char('"') => self.split_editor(Axis::Vertical)?,
            KeyCode::Char('c') => self.open_terminal(None, Some(Axis::Vertical))?,
            KeyCode::Char('a') => self.open_ai(Some(Axis::Horizontal))?,
            KeyCode::Char('A') => self.open_ai(Some(Axis::Vertical))?,
            KeyCode::Char('e') => {
                self.sidebar_visible = true;
                self.sidebar_focus = !self.sidebar_focus;
            }
            KeyCode::Char('s') => {
                self.sidebar_visible = !self.sidebar_visible;
                self.sidebar_focus = false;
            }
            KeyCode::Char('g') => {
                self.sidebar_visible = true;
                self.sidebar_focus = true;
                self.git_focus = true;
            }
            KeyCode::Char('G') => self.git_pane()?,
            KeyCode::Char('z') => {
                self.zoom = !self.zoom;
                self.sidebar_focus = false;
            }
            KeyCode::Char('x') => self.close(false)?,
            KeyCode::Char('q') => self.quit(false)?,
            KeyCode::Char('m') => self.toggle_preview()?,
            KeyCode::Char('t') => self.open_picker(),
            KeyCode::Char('M') => self.split_preview()?,
            KeyCode::Char('?') => self.toggle_help(),
            KeyCode::Char(':') => self.input_start(InputKind::Command),
            KeyCode::Char('o') | KeyCode::Tab => self.cycle(),
            KeyCode::Char('b') => {
                if let Some(Pane::Terminal(t)) = self.panes.get_mut(&self.focused) {
                    t.send(&[2])?;
                }
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    let axis = if matches!(key.code, KeyCode::Left | KeyCode::Right) {
                        Axis::Horizontal
                    } else {
                        Axis::Vertical
                    };
                    let delta = if matches!(key.code, KeyCode::Left | KeyCode::Up) {
                        -5
                    } else {
                        5
                    };
                    self.tiles.resize(self.focused, axis, delta);
                } else {
                    self.move_focus(key.code);
                }
            }
            KeyCode::Char('h') => self.move_focus(KeyCode::Left),
            KeyCode::Char('l') => self.move_focus(KeyCode::Right),
            KeyCode::Char('k') => self.move_focus(KeyCode::Up),
            KeyCode::Char('j') => self.move_focus(KeyCode::Down),
            KeyCode::PageUp | KeyCode::PageDown => {
                if let Some(Pane::Terminal(t)) = self.panes.get_mut(&self.focused) {
                    t.scroll(if key.code == KeyCode::PageUp { 10 } else { -10 });
                }
            }
            KeyCode::Esc => {}
            _ => self.notice("Ctrl-b: % split · \" stack · c shell · a AI · arrows focus · ? help"),
        }
        Ok(())
    }
    fn cycle(&mut self) {
        let ids: Vec<usize> = self.panes.keys().copied().collect();
        let index = ids.iter().position(|id| *id == self.focused).unwrap_or(0);
        self.focused = ids[(index + 1) % ids.len()];
        self.sidebar_focus = false;
        self.mode = Mode::Normal;
    }
    fn move_focus(&mut self, direction: KeyCode) {
        let Some((_, current)) = self.rectangles.iter().find(|(id, _)| *id == self.focused) else {
            return;
        };
        let x = current.x as i32 + current.width as i32 / 2;
        let y = current.y as i32 + current.height as i32 / 2;
        let mut best = None;
        for (id, rect) in &self.rectangles {
            if *id == self.focused {
                continue;
            }
            let dx = rect.x as i32 + rect.width as i32 / 2 - x;
            let dy = rect.y as i32 + rect.height as i32 / 2 - y;
            let valid = match direction {
                KeyCode::Left => dx < 0,
                KeyCode::Right => dx > 0,
                KeyCode::Up => dy < 0,
                _ => dy > 0,
            };
            if valid {
                let score = if matches!(direction, KeyCode::Left | KeyCode::Right) {
                    dx.abs() + 2 * dy.abs()
                } else {
                    2 * dx.abs() + dy.abs()
                };
                if best.is_none_or(|(_, s)| score < s) {
                    best = Some((*id, score));
                }
            }
        }
        if let Some((id, _)) = best {
            self.focused = id;
            self.sidebar_focus = false;
            self.mode = Mode::Normal;
        }
    }
    fn sidebar_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.code == KeyCode::Tab {
            self.git_focus = !self.git_focus;
            return Ok(());
        }
        if self.git_focus {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.git_selected = self.git_selected.saturating_sub(1);
                    self.peek_selected();
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.git_selected =
                        (self.git_selected + 1).min(self.git.changes.len().saturating_sub(1));
                    self.peek_selected();
                }
                KeyCode::Enter => {
                    if let Some((_, path)) = self.git.changes.get(self.git_selected) {
                        let path = self.root.join(path);
                        self.open(&path)?;
                    }
                }
                KeyCode::Esc => self.sidebar_focus = false,
                KeyCode::Char(':') => self.input_start(InputKind::Command),
                _ => {}
            }
            return Ok(());
        }
        let path = match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.tree.down(-1);
                self.peek_selected();
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.tree.down(1);
                self.peek_selected();
                None
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.tree.left()?;
                None
            }
            KeyCode::Right | KeyCode::Char('l') => self.tree.right()?,
            KeyCode::Enter => self.tree.activate()?,
            KeyCode::Char('r') => {
                self.tree.refresh()?;
                self.notice("File tree refreshed");
                None
            }
            KeyCode::Char('n') => {
                self.input = Some(Input {
                    kind: InputKind::Command,
                    text: "e ".into(),
                });
                None
            }
            KeyCode::Esc => {
                self.sidebar_focus = false;
                None
            }
            KeyCode::Char(':') => {
                self.input_start(InputKind::Command);
                None
            }
            _ => None,
        };
        if let Some(path) = path {
            self.open(&path)?;
        }
        Ok(())
    }
    fn insert_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.menu.is_some() {
            match key.code {
                KeyCode::Up | KeyCode::Char('p') if key.code == KeyCode::Up || ctrl => {
                    self.menu_move(-1);
                    return Ok(());
                }
                KeyCode::Down | KeyCode::Char('n') if key.code == KeyCode::Down || ctrl => {
                    self.menu_move(1);
                    return Ok(());
                }
                KeyCode::Tab | KeyCode::Enter => {
                    self.menu_accept();
                    return Ok(());
                }
                KeyCode::Esc => {
                    self.menu = None;
                    return Ok(());
                }
                _ => {}
            }
        }
        if ctrl && matches!(key.code, KeyCode::Char(' ') | KeyCode::Char('@')) {
            return self.lsp_request(lsp::Kind::Completion);
        }
        let typed = match key.code {
            KeyCode::Char(c) if !ctrl => Some(c),
            _ => None,
        };
        self.insert_edit(key)?;
        if self.menu.is_some() {
            self.menu_filter();
        }
        if let (Some(c), Some(id)) = (typed, self.buffer_id()) {
            if self.menu.is_none() && self.lsp.is_trigger(id, c) {
                let _ = self.lsp_request(lsp::Kind::Completion);
            }
        }
        Ok(())
    }
    fn insert_edit(&mut self, key: KeyEvent) -> Result<()> {
        let width = self.config.tab_width;
        let Some(buffer) = self.active_buffer() else {
            return Ok(());
        };
        match key.code {
            KeyCode::Esc => {
                buffer.end_group();
                self.mode = Mode::Normal;
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                buffer.insert(&c.to_string())
            }
            KeyCode::Enter => buffer.newline(),
            KeyCode::Tab => buffer.insert(&" ".repeat(width)),
            KeyCode::Backspace => buffer.backspace(),
            KeyCode::Delete => buffer.delete_char(),
            KeyCode::Left => {
                buffer.end_group();
                buffer.move_by(-1, 0);
            }
            KeyCode::Right => {
                buffer.end_group();
                buffer.move_by(1, 0);
            }
            KeyCode::Up => {
                buffer.end_group();
                buffer.move_by(0, -1);
            }
            KeyCode::Down => {
                buffer.end_group();
                buffer.move_by(0, 1);
            }
            KeyCode::Home => buffer.home(),
            KeyCode::End => buffer.end(),
            KeyCode::PageUp => buffer.move_by(0, -20),
            KeyCode::PageDown => buffer.move_by(0, 20),
            _ => {}
        }
        Ok(())
    }
    fn normal_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Char(':') {
            self.input_start(InputKind::Command);
            return Ok(());
        }
        if key.code == KeyCode::Char('/') {
            self.input_start(InputKind::Search);
            return Ok(());
        }
        if key.code == KeyCode::Tab {
            self.next_buffer(false);
            return Ok(());
        }
        if key.code == KeyCode::BackTab {
            self.next_buffer(true);
            return Ok(());
        }
        if key.code == KeyCode::Esc {
            self.pending = None;
            self.count.clear();
            return Ok(());
        }
        if let KeyCode::Char(c) = key.code {
            if c.is_ascii_digit() && (c != '0' || !self.count.is_empty()) && !ctrl {
                if self.count.len() < 6 {
                    self.count.push(c);
                }
                // Relative numbers show distances; say how to use the one typed.
                if self.config.relative_numbers {
                    let n = &self.count;
                    self.notice(format!(
                        "{n} · j/↓/Enter: {n} lines down · k/↑/-: {n} lines up · G: line {n}"
                    ));
                }
                return Ok(());
            }
        }
        let count = self.count.parse::<usize>().unwrap_or(1).clamp(1, 100_000);
        let has_count = !self.count.is_empty();
        self.count.clear();
        let pending = self.pending.take();
        let Some(id) = self.buffer_id() else {
            return Ok(());
        };
        let b = &mut self.buffers[id];
        if ctrl {
            match key.code {
                KeyCode::Char('r') => b.undo(true),
                KeyCode::Char('o') => return self.jump_back(),
                KeyCode::Char('d') => b.move_by(0, 15),
                KeyCode::Char('u') => b.move_by(0, -15),
                _ => {}
            }
            return Ok(());
        }
        if let Some(pending) = pending {
            match (pending, key.code) {
                ('g', KeyCode::Char('g')) => b.goto(if has_count { count } else { 1 }),
                ('g', KeyCode::Char('d')) => return self.lsp_request(lsp::Kind::Definition),
                (']', KeyCode::Char('d')) => self.next_diagnostic(false),
                ('[', KeyCode::Char('d')) => self.next_diagnostic(true),
                ('d', KeyCode::Char('d')) => {
                    let line = b.delete_line();
                    self.clipboard.copy(line, true);
                }
                ('y', KeyCode::Char('y')) => {
                    let line = format!("{}\n", b.line(b.row));
                    self.clipboard.copy(line, true);
                    self.notice("Line copied · p pastes below, P above");
                }
                _ => {}
            }
            return Ok(());
        }
        match key.code {
            KeyCode::Char('h') | KeyCode::Left => b.move_by(-(count as isize), 0),
            KeyCode::Char('l') | KeyCode::Right => b.move_by(count as isize, 0),
            KeyCode::Char('j') | KeyCode::Down => b.move_by(0, count as isize),
            KeyCode::Char('k') | KeyCode::Up => b.move_by(0, -(count as isize)),
            KeyCode::Char('w') => {
                for _ in 0..count {
                    b.word(false);
                }
            }
            KeyCode::Char('b') => {
                for _ in 0..count {
                    b.word(true);
                }
            }
            KeyCode::Char('0') | KeyCode::Home => b.home(),
            KeyCode::Char('$') | KeyCode::End => b.end(),
            KeyCode::Char('^') => {
                b.col = b
                    .line(b.row)
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .count()
            }
            KeyCode::Char('G') => b.goto(if has_count { count } else { b.lines() }),
            // Vim's relative jumps: 15<Enter> / 15+ down, 15- up, to the first non-blank.
            KeyCode::Enter | KeyCode::Char('+') | KeyCode::Char('-') => {
                let up = key.code == KeyCode::Char('-');
                b.move_by(
                    0,
                    if up {
                        -(count as isize)
                    } else {
                        count as isize
                    },
                );
                b.col = b
                    .line(b.row)
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .count();
            }
            KeyCode::Char('K') => return self.lsp_request(lsp::Kind::Hover),
            KeyCode::Char('g')
            | KeyCode::Char('d')
            | KeyCode::Char('y')
            | KeyCode::Char('[')
            | KeyCode::Char(']') => {
                if let KeyCode::Char(c) = key.code {
                    self.pending = Some(c);
                    if has_count {
                        self.count = count.to_string();
                    }
                }
            }
            KeyCode::Char('i') => {
                b.end_group();
                self.mode = Mode::Insert;
            }
            KeyCode::Char('a') => {
                b.move_by(1, 0);
                b.end_group();
                self.mode = Mode::Insert;
            }
            KeyCode::Char('A') => {
                b.end();
                b.end_group();
                self.mode = Mode::Insert;
            }
            KeyCode::Char('I') => {
                b.home();
                b.end_group();
                self.mode = Mode::Insert;
            }
            KeyCode::Char('o') => {
                b.open_line(false);
                self.mode = Mode::Insert;
            }
            KeyCode::Char('O') => {
                b.open_line(true);
                self.mode = Mode::Insert;
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                for _ in 0..count.min(b.line_len(b.row).saturating_sub(b.col)) {
                    b.delete_char();
                }
            }
            KeyCode::Char('D') => b.delete_to_end(),
            KeyCode::Char('C') => {
                b.delete_to_end();
                self.mode = Mode::Insert;
            }
            KeyCode::Char('p') => return self.paste(false),
            KeyCode::Char('P') => return self.paste(true),
            KeyCode::Char('v') => self.start_visual(Mode::Visual),
            KeyCode::Char('V') => self.start_visual(Mode::VisualLine),
            KeyCode::Char('u') => b.undo(false),
            KeyCode::Char('n') => {
                if !b.find(false) {
                    self.notice("No match");
                }
            }
            KeyCode::Char('N') => {
                if !b.find(true) {
                    self.notice("No match");
                }
            }
            KeyCode::PageUp => b.move_by(0, -20),
            KeyCode::PageDown => b.move_by(0, 20),
            _ => {}
        }
        Ok(())
    }
    fn lsp_request(&mut self, kind: lsp::Kind) -> Result<()> {
        let id = self.buffer_id().context("Focus an editor")?;
        self.lsp
            .request(kind, id, &self.buffers[id], self.config.tab_width)?;
        if kind == lsp::Kind::Format {
            self.notice("Formatting…");
        }
        Ok(())
    }
    fn lsp_event(&mut self, event: lsp::Event) {
        match event {
            lsp::Event::Notice(text) => self.notice(text),
            lsp::Event::Error(text) => self.fail(text),
            lsp::Event::Hover(text) => self.hover = Some(text),
            lsp::Event::Definition {
                path,
                line,
                character,
                utf32,
            } => {
                let Some(id) = self.buffer_id() else {
                    return;
                };
                let b = &self.buffers[id];
                let here = (b.path.clone(), b.row, b.col);
                if let Err(error) = self.open(&path) {
                    return self.fail(error);
                }
                if let (Some(path), row, col) = here {
                    self.jumps.push((path, row, col));
                }
                if let Some(b) = self.active_buffer() {
                    b.row = line.min(b.lines() - 1);
                    b.col = lsp::decode(&b.line(b.row), character, utf32);
                    b.scroll = b.row.saturating_sub(5);
                }
                self.notice("Jumped to definition · Ctrl-o returns");
            }
            lsp::Event::Completion { buffer, row, items } => {
                let Some(b) = self.buffers.get(buffer) else {
                    return;
                };
                if self.mode != Mode::Insert || self.buffer_id() != Some(buffer) || b.row != row {
                    return;
                }
                if items.is_empty() {
                    return self.notice("No completions");
                }
                let line: Vec<char> = b.line(row).chars().collect();
                let mut start = b.col.min(line.len());
                while start > 0 && (line[start - 1].is_alphanumeric() || line[start - 1] == '_') {
                    start -= 1;
                }
                self.menu = Some(Menu {
                    shown: (0..items.len()).collect(),
                    items,
                    selected: 0,
                    buffer,
                    row,
                    start,
                });
                self.menu_filter();
            }
            lsp::Event::Format { buffer, edits } => {
                let count = edits.len();
                if let Some(b) = self.buffers.get_mut(buffer) {
                    b.apply_edits(edits);
                }
                self.notice(if count == 0 {
                    "Already formatted".to_string()
                } else {
                    format!(
                        "Formatted · {count} edit{}",
                        if count == 1 { "" } else { "s" }
                    )
                });
            }
        }
    }
    /// Narrow the completion menu to what has been typed since it opened.
    fn menu_filter(&mut self) {
        let focused = self.buffer_id();
        let Some(menu) = &mut self.menu else {
            return;
        };
        let b = &self.buffers[menu.buffer];
        if focused != Some(menu.buffer) || b.row != menu.row || b.col < menu.start {
            self.menu = None;
            return;
        }
        let prefix: String = b
            .line(b.row)
            .chars()
            .skip(menu.start)
            .take(b.col - menu.start)
            .collect();
        if prefix.chars().any(|c| !(c.is_alphanumeric() || c == '_')) {
            self.menu = None;
            return;
        }
        menu.shown = (0..menu.items.len())
            .filter(|&i| menu.items[i].matches(&prefix))
            .collect();
        menu.selected = 0;
        if menu.shown.is_empty() {
            self.menu = None;
        }
    }
    fn menu_move(&mut self, delta: isize) {
        if let Some(menu) = &mut self.menu {
            let len = menu.shown.len() as isize;
            menu.selected = (menu.selected as isize + delta).rem_euclid(len) as usize;
        }
    }
    fn menu_accept(&mut self) {
        let Some(menu) = self.menu.take() else {
            return;
        };
        let item = &menu.items[menu.shown[menu.selected]];
        let b = &mut self.buffers[menu.buffer];
        let start_col = item.start.unwrap_or(menu.start).min(b.col);
        let start = b.char_index(menu.row, start_col);
        let end = b.index();
        b.replace(start, end, &item.insert);
    }
    fn next_diagnostic(&mut self, backwards: bool) {
        let Some(id) = self.buffer_id() else {
            return;
        };
        let rows: Vec<usize> = self
            .lsp
            .diagnostics
            .get(&id)
            .map(|d| d.iter().map(|d| d.row).collect())
            .unwrap_or_default();
        let b = &mut self.buffers[id];
        let target = if backwards {
            rows.iter().rev().find(|&&r| r < b.row).or(rows.last())
        } else {
            rows.iter().find(|&&r| r > b.row).or(rows.first())
        };
        match target {
            Some(&row) => {
                b.row = row;
                b.col = 0;
                b.clamp();
            }
            None => self.notice("No diagnostics in this file"),
        }
    }
    fn jump_back(&mut self) -> Result<()> {
        let (path, row, col) = self.jumps.pop().context("No earlier position")?;
        self.open(&path)?;
        if let Some(b) = self.active_buffer() {
            b.row = row;
            b.col = col;
            b.clamp();
        }
        Ok(())
    }
    fn markdown_buffer(&self, id: usize) -> Result<()> {
        if self.buffers[id]
            .path
            .as_deref()
            .is_some_and(markdown::is_markdown)
        {
            Ok(())
        } else {
            bail!("Preview renders Markdown files (.md, .markdown)")
        }
    }
    /// Switch the focused pane between rendered Markdown and its source, keeping place.
    fn toggle_preview(&mut self) -> Result<()> {
        match self.panes.get(&self.focused) {
            Some(Pane::Editor(id)) => {
                let id = *id;
                self.markdown_buffer(id)?;
                self.buffers[id].end_group();
                self.panes
                    .insert(self.focused, Pane::Preview(Preview::new(id)));
                self.mode = Mode::Normal;
                self.notice("Markdown preview · e edits the source · [ ] jump between headings");
            }
            Some(Pane::Preview(p)) => {
                let id = p.buffer;
                if let Some(doc) = self.previews.get(id) {
                    let b = &mut self.buffers[id];
                    b.row = doc.source_for_line(p.scroll + 1).min(b.lines() - 1);
                    b.col = 0;
                    b.scroll = b.row.saturating_sub(2);
                }
                self.panes.insert(self.focused, Pane::Editor(id));
                self.notice("Editing Markdown source · Ctrl-b m or :preview renders it");
            }
            _ => bail!("Focus a Markdown file to preview it"),
        }
        Ok(())
    }
    /// Live preview beside the source; focus stays in the editor.
    fn split_preview(&mut self) -> Result<()> {
        let id = match self.panes.get(&self.focused) {
            Some(Pane::Editor(id)) => *id,
            Some(Pane::Preview(p)) => {
                let id = p.buffer;
                self.panes.insert(self.focused, Pane::Editor(id));
                id
            }
            _ => bail!("Focus a Markdown file to preview it"),
        };
        self.markdown_buffer(id)?;
        if self.panes.len() >= 12 {
            bail!("Maximum of 12 panes");
        }
        let editor = self.focused;
        self.add_pane(Pane::Preview(Preview::new(id)), Axis::Horizontal);
        self.focused = editor;
        self.notice("Live preview: it follows your cursor as you edit");
        Ok(())
    }
    /// Open the Git pane docked full-width along the bottom; when it is open, focus
    /// it, and when it is already focused, maximize or restore it.
    fn git_pane(&mut self) -> Result<()> {
        let existing = self
            .panes
            .iter()
            .find(|(_, p)| matches!(p, Pane::Git(_)))
            .map(|(id, _)| *id);
        match existing {
            Some(id) if id == self.focused && !self.sidebar_focus => {
                self.zoom = !self.zoom;
            }
            Some(id) => {
                self.focused = id;
                self.sidebar_focus = false;
                self.zoom = false;
            }
            None => {
                if self.panes.len() >= 12 {
                    bail!("Maximum of 12 panes");
                }
                let id = self.next_pane;
                self.next_pane += 1;
                self.panes
                    .insert(id, Pane::Git(Box::new(GitView::new(self.root.clone()))));
                let above = std::mem::replace(&mut self.tiles, Tile::Leaf(id));
                self.tiles = Tile::Split {
                    axis: Axis::Vertical,
                    percent: 60,
                    first: Box::new(above),
                    second: Box::new(Tile::Leaf(id)),
                };
                self.focused = id;
                self.sidebar_focus = false;
                self.zoom = false;
                self.notice("Git: Tab branches/commits · Enter show branch · Ctrl-b G maximize");
            }
        }
        self.mode = Mode::Normal;
        Ok(())
    }
    fn git_pane_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(Pane::Git(view)) = self.panes.get_mut(&self.focused) else {
            return Ok(());
        };
        let page = view.height.max(2) as isize;
        match key.code {
            KeyCode::Char('d') if ctrl => view.step(page / 2),
            KeyCode::Char('u') if ctrl => view.step(-page / 2),
            _ if ctrl => {}
            KeyCode::Tab | KeyCode::BackTab => {
                view.focus = match view.focus {
                    gitgraph::Focus::Commits => gitgraph::Focus::Branches,
                    gitgraph::Focus::Branches => gitgraph::Focus::Commits,
                };
            }
            KeyCode::Char('j') | KeyCode::Down => view.step(1),
            KeyCode::Char('k') | KeyCode::Up => view.step(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => view.step(page - 1),
            KeyCode::PageUp => view.step(1 - page),
            KeyCode::Char('g') | KeyCode::Home => view.step(isize::MIN / 2),
            KeyCode::Char('G') | KeyCode::End => view.step(isize::MAX / 2),
            KeyCode::Enter => match view.focus {
                gitgraph::Focus::Branches => view.toggle_only(),
                gitgraph::Focus::Commits => view.details = !view.details,
            },
            KeyCode::Char('d') => view.details = !view.details,
            KeyCode::Char('a') | KeyCode::Esc => view.show_all(),
            KeyCode::Char('r') => view.refresh(),
            KeyCode::Char('y') => {
                if let Some(hash) = view.selected().map(|c| c.hash.clone()) {
                    self.clipboard.copy(hash.clone(), false);
                    self.notice(format!("Copied {hash}"));
                }
            }
            KeyCode::Char(':') => self.input_start(InputKind::Command),
            _ => {}
        }
        Ok(())
    }
    fn preview_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(Pane::Preview(p)) = self.panes.get_mut(&self.focused) else {
            return Ok(());
        };
        let page = p.height.max(2);
        let headings = self
            .previews
            .get(p.buffer)
            .map(|d| d.headings.clone())
            .unwrap_or_default();
        match key.code {
            KeyCode::Char('d') if ctrl => p.scroll += page / 2,
            KeyCode::Char('u') if ctrl => p.scroll = p.scroll.saturating_sub(page / 2),
            KeyCode::Char('f') if ctrl => p.scroll += page,
            KeyCode::Char('b') if ctrl => p.scroll = p.scroll.saturating_sub(page),
            _ if ctrl => {}
            KeyCode::Char('j') | KeyCode::Down => p.scroll += 1,
            KeyCode::Char('k') | KeyCode::Up => p.scroll = p.scroll.saturating_sub(1),
            KeyCode::Char(' ') | KeyCode::PageDown => p.scroll += page.saturating_sub(2),
            KeyCode::PageUp => p.scroll = p.scroll.saturating_sub(page.saturating_sub(2)),
            KeyCode::Char('g') | KeyCode::Home => p.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => p.scroll = usize::MAX / 2,
            KeyCode::Char(']') => {
                if let Some(&line) = headings.iter().find(|&&h| h > p.scroll) {
                    p.scroll = line;
                }
            }
            KeyCode::Char('[') => {
                if let Some(&line) = headings.iter().rev().find(|&&h| h < p.scroll) {
                    p.scroll = line;
                }
            }
            KeyCode::Char('e') | KeyCode::Char('i') | KeyCode::Enter => self.toggle_preview()?,
            KeyCode::Char('/') => {
                self.toggle_preview()?;
                self.input_start(InputKind::Search);
            }
            KeyCode::Char(':') => self.input_start(InputKind::Command),
            KeyCode::Tab | KeyCode::BackTab => {
                self.toggle_preview()?;
                self.next_buffer(key.code == KeyCode::BackTab);
            }
            _ => {}
        }
        Ok(())
    }
    /// Default placement for AI panes, from `ai_split` in config.toml.
    fn ai_axis(&self) -> Axis {
        match self.config.ai_split.as_str() {
            "below" | "stacked" | "down" => Axis::Vertical,
            _ => Axis::Horizontal,
        }
    }
    /// Open the AI launcher pane beside (Horizontal) or below (Vertical) the focus.
    fn open_ai(&mut self, axis: Option<Axis>) -> Result<()> {
        if self.panes.len() >= 12 {
            bail!("Maximum of 12 panes");
        }
        let axis = axis.unwrap_or_else(|| self.ai_axis());
        self.add_pane(Pane::Ai, axis);
        self.notice(
            "AI launcher: 1 Codex · 2 Claude · 3 Gemini · 4 Antigravity · 5 Local (Ollama)",
        );
        Ok(())
    }
    /// Run an AI CLI in the focused AI pane (`split: None`) or in a new split.
    fn launch_agent(&mut self, program: &str, split: Option<Axis>) -> Result<()> {
        self.launch(program, None, split)
    }
    /// Your own `[ai.<name>]` launchers that are not built-in names, in name order.
    pub fn custom_agents(&self) -> Vec<String> {
        self.config
            .ai
            .keys()
            .filter(|name| !AGENTS.iter().any(|a| a.1 == name.as_str()))
            .cloned()
            .collect()
    }
    fn launch(&mut self, program: &str, model: Option<&str>, split: Option<Axis>) -> Result<()> {
        let ((command, args, env), _) = self.agent_command(program, model, false)?;
        self.open_terminal_with(Some((&command, args)), &env, split)?;
        if let Some(Pane::Terminal(t)) = self.panes.get_mut(&self.focused) {
            t.launch = Launch::Agent {
                name: program.into(),
                model: model.map(String::from),
            };
        }
        Ok(())
    }
    /// What to run for an AI CLI: (program, args, env, resumed). With `resume`, the
    /// built-in CLIs reopen their latest conversation in this project.
    fn agent_command(
        &self,
        program: &str,
        model: Option<&str>,
        resume: bool,
    ) -> Result<(crate::local_ai::Run, bool)> {
        use crate::local_ai;
        if program == "local" {
            let (program, args, env) = local_ai::local(
                self.config.ai.get("local"),
                model,
                local_ai::ollama_models,
                crate::lsp::on_path,
            )?;
            return Ok(((program, args, env), false));
        }
        // An [ai.<name>] entry, which may also override a built-in like `codex`.
        if let Some(launcher) = self.config.ai.get(program).cloned() {
            if !crate::lsp::on_path(&launcher.program) {
                bail!("`{}` from [ai.{program}] is not on PATH", launcher.program);
            }
            return Ok((local_ai::command(&launcher, model)?, false));
        }
        let (_, _, name, hint) = AGENTS.iter().find(|a| a.1 == program).copied().unwrap_or((
            ' ',
            program,
            program,
            "see its documentation",
        ));
        if !crate::lsp::on_path(program) {
            bail!("{name} is not installed (`{program}` not on PATH) · install: {hint}");
        }
        let continue_flags: &[&str] = match program {
            "claude" | "agy" => &["--continue"],
            "codex" => &["resume", "--last"],
            "gemini" => &["--resume", "latest"],
            _ => &[],
        };
        let flags = if resume { continue_flags } else { &[] };
        let args = flags.iter().map(|f| f.to_string()).collect();
        Ok(((program.into(), args, Vec::new()), !flags.is_empty()))
    }
    fn toggle_help(&mut self) {
        self.help = !self.help;
        self.help_scroll = 0;
        if self.help {
            self.notice("Field guide open beside your work · F1 closes · Alt-↑↓ scrolls");
        }
    }
    pub fn open_picker(&mut self) {
        let themes: Vec<(String, &'static str)> = config::available_themes()
            .into_iter()
            .map(|name| {
                let origin = config::theme_origin(&name);
                (name, origin)
            })
            .collect();
        let original = self.config.theme.clone();
        let selected = themes.iter().position(|(n, _)| *n == original).unwrap_or(0);
        self.picker = Some(ThemePicker {
            shown: (0..themes.len()).collect(),
            themes,
            selected,
            filter: String::new(),
            original,
        });
        self.notice(format!(
            "Themes: ↑↓ preview · type to filter · Enter keeps · Esc cancels · add your own in {}",
            config::themes_dir().display()
        ));
    }
    fn picker_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(picker) = &mut self.picker else {
            return Ok(());
        };
        let len = picker.shown.len();
        let mut refilter = false;
        match key.code {
            KeyCode::Esc => {
                let original = picker.original.clone();
                self.picker = None;
                self.apply_theme(&original)?;
                self.notice(format!("Theme unchanged: {original}"));
                return Ok(());
            }
            KeyCode::Enter => {
                let chosen = picker
                    .shown
                    .get(picker.selected)
                    .map(|&i| picker.themes[i].0.clone());
                self.picker = None;
                if let Some(name) = chosen {
                    self.command(&format!("theme {name}"))?;
                }
                return Ok(());
            }
            KeyCode::Up | KeyCode::BackTab if len > 0 => {
                picker.selected = (picker.selected + len - 1) % len;
            }
            KeyCode::Down | KeyCode::Tab if len > 0 => {
                picker.selected = (picker.selected + 1) % len;
            }
            KeyCode::PageUp => picker.selected = picker.selected.saturating_sub(10),
            KeyCode::PageDown if len > 0 => picker.selected = (picker.selected + 10).min(len - 1),
            KeyCode::Home => picker.selected = 0,
            KeyCode::End => picker.selected = len.saturating_sub(1),
            KeyCode::Backspace => {
                picker.filter.pop();
                refilter = true;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                picker.filter.push(c);
                refilter = true;
            }
            _ => return Ok(()),
        }
        if refilter {
            let filter = picker.filter.to_lowercase();
            picker.shown = (0..picker.themes.len())
                .filter(|&i| picker.themes[i].0.to_lowercase().contains(&filter))
                .collect();
            picker.selected = 0;
        }
        // Live preview of the highlighted theme; not saved until Enter.
        let preview = picker
            .shown
            .get(picker.selected)
            .map(|&i| picker.themes[i].0.clone());
        if let Some(name) = preview {
            if name != self.config.theme {
                if let Err(error) = self.apply_theme(&name) {
                    self.fail(format!("{name}: {error:#}"));
                }
            }
        }
        Ok(())
    }
    fn apply_theme(&mut self, name: &str) -> Result<()> {
        let theme = config::load_theme(name)?;
        self.syntax = Syntax::new(&theme);
        self.previews.clear();
        for pane in self.panes.values_mut() {
            if let Pane::Terminal(terminal) = pane {
                terminal.set_theme(&theme);
            }
        }
        self.theme = theme;
        self.config.theme = name.into();
        self.theme_stamp = config::theme_stamp(name);
        self.redraw = true;
        Ok(())
    }
    fn next_buffer(&mut self, backwards: bool) {
        if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
            self.end_visual();
        }
        if let Some(id) = self.buffer_id() {
            let id = if backwards {
                (id + self.buffers.len() - 1) % self.buffers.len()
            } else {
                (id + 1) % self.buffers.len()
            };
            self.panes.insert(self.focused, Pane::Editor(id));
        }
    }
    fn input_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.code != KeyCode::Tab {
            self.completion = None;
        }
        match key.code {
            KeyCode::Esc => {
                self.input = None;
            }
            KeyCode::Backspace => {
                self.input.as_mut().expect("input active").text.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.as_mut().expect("input active").text.push(c)
            }
            KeyCode::Enter => {
                let input = self.input.take().expect("input active");
                match input.kind {
                    InputKind::Command => self.command(input.text.trim())?,
                    InputKind::Search => {
                        if let Some(b) = self.active_buffer() {
                            b.search = input.text;
                            if !b.find(false) {
                                self.notice("No match");
                            }
                        }
                    }
                    InputKind::Open => {
                        if !input.text.is_empty() {
                            self.open(Path::new(&input.text))?;
                        }
                    }
                }
            }
            KeyCode::Tab => {
                let text = self.input.as_ref().expect("input active").text.clone();
                let kind = self.input.as_ref().expect("input active").kind;
                if kind == InputKind::Command && text.starts_with("theme ") {
                    // Repeated Tab cycles through every theme matching what was typed.
                    let (prefix, index) = match self.completion.take() {
                        Some((prefix, index)) => (prefix, index + 1),
                        None => (text["theme ".len()..].trim().to_string(), 0),
                    };
                    let matches: Vec<String> = config::available_themes()
                        .into_iter()
                        .filter(|n| n.starts_with(&prefix))
                        .collect();
                    if !matches.is_empty() {
                        let name = &matches[index % matches.len()];
                        self.input.as_mut().expect("input active").text = format!("theme {name}");
                        self.completion = Some((prefix, index));
                    }
                    return Ok(());
                }
                if kind == InputKind::Open {
                    let mut matches = self
                        .tree
                        .entries
                        .iter()
                        .filter_map(|e| {
                            e.path
                                .strip_prefix(&self.root)
                                .ok()
                                .map(|p| p.to_string_lossy().into_owned())
                        })
                        .filter(|p| p.starts_with(&text));
                    if let Some(found) = matches.next() {
                        self.input.as_mut().expect("input active").text = found;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub fn command(&mut self, command: &str) -> Result<()> {
        // :+15 / :-15 move relative to the cursor line.
        if let Some(offset) = command
            .strip_prefix('+')
            .or_else(|| command.strip_prefix('-'))
            .and_then(|n| n.parse::<isize>().ok())
        {
            let delta = if command.starts_with('-') {
                -offset
            } else {
                offset
            };
            if let Some(b) = self.active_buffer() {
                b.move_by(0, delta);
            }
            return Ok(());
        }
        if let Ok(line) = command.parse::<usize>() {
            if let Some(b) = self.active_buffer() {
                b.goto(line);
            }
            return Ok(());
        }
        let (verb, arg) = command
            .split_once(char::is_whitespace)
            .map(|(a, b)| (a, b.trim()))
            .unwrap_or((command, ""));
        match verb {
            "w" | "write" => self.save((!arg.is_empty()).then(|| self.root.join(arg)))?,
            "wa" => {
                for (id, buffer) in self.buffers.iter_mut().enumerate() {
                    if buffer.dirty {
                        buffer.save(None)?;
                        self.lsp.saved(id, buffer);
                    }
                }
                self.notice("Saved all modified buffers");
            }
            "wq" => {
                self.save(None)?;
                self.close(false)?;
            }
            "q" => self.close(false)?,
            "q!" => self.close(true)?,
            "qa" => self.quit(false)?,
            "qa!" => self.quit(true)?,
            "e" | "edit" => {
                if arg.is_empty() {
                    bail!("Usage: :e path (creates a new file buffer if absent)");
                }
                self.open(Path::new(arg))?;
            }
            "new" => {
                self.buffers.push(Buffer::empty());
                self.add_pane(Pane::Editor(self.buffers.len() - 1), Axis::Horizontal);
            }
            "reload" => {
                let id = self.buffer_id().context("Focus an editor")?;
                if self.buffers[id].dirty {
                    bail!("Unsaved changes; save to another name first");
                }
                let path = self.buffers[id].path.clone().context("Unnamed buffer")?;
                let mut fresh = Buffer::open(&path)?;
                fresh.revision = self.buffers[id].revision + 1;
                self.buffers[id] = fresh;
            }
            "preview" | "md" => match arg {
                "" => self.toggle_preview()?,
                "split" | "vs" => self.split_preview()?,
                _ => bail!("Use :preview or :preview split"),
            },
            "format" | "fmt" => self.lsp_request(lsp::Kind::Format)?,
            "lsp" => match arg {
                "" => self.notice(self.lsp.report()),
                "restart" => {
                    self.lsp.restart();
                    self.notice("Language servers restarting");
                }
                _ => bail!("Use :lsp or :lsp restart"),
            },
            "vsplit" | "vs" => self.split_editor(Axis::Horizontal)?,
            "split" | "sp" => self.split_editor(Axis::Vertical)?,
            "term" | "terminal" => self.open_terminal(None, Some(Axis::Vertical))?,
            "session" => match arg {
                "" => self.notice(if !self.config.restore_session {
                    "Sessions are off (restore_session = false)".to_string()
                } else if !self.session_enabled {
                    "Session saving is off until restart; :session save turns it back on".into()
                } else if session::wanted(&self.root) {
                    format!(
                        "Session saves to {} and reopens next time",
                        session::file(&self.root).display()
                    )
                } else {
                    "No session here yet; :session save keeps this workspace for next time".into()
                }),
                "save" => {
                    self.session_enabled = true;
                    self.save_session(true)?;
                    self.notice(format!(
                        "Session saved to {}",
                        session::file(&self.root).display()
                    ));
                }
                "forget" | "clear" => {
                    session::forget(&self.root)?;
                    self.session_enabled = false;
                    self.session_json.clear();
                    self.notice(
                        "Session deleted; Odin will start fresh here (:session save undoes)",
                    );
                }
                _ => bail!("Use :session, :session save or :session forget"),
            },
            "git" | "graph" => match arg {
                "" => self.git_pane()?,
                "all" => {
                    self.git_pane()?;
                    if let Some(Pane::Git(view)) = self.panes.get_mut(&self.focused) {
                        view.show_all();
                    }
                }
                "side" | "sidebar" => {
                    self.sidebar_visible = true;
                    self.sidebar_focus = true;
                    self.git_focus = true;
                }
                _ => bail!("Use :git (open / focus / zoom the Git pane), :git all or :git side"),
            },
            // :ai [codex|claude|gemini|agy|local|<[ai.name]>] [model] [right|below]
            "ai" => {
                let mut program = None;
                let mut axis = None;
                let mut model = None;
                for word in arg.split_whitespace() {
                    match word {
                        "right" | "beside" | "side" => axis = Some(Axis::Horizontal),
                        "below" | "stacked" | "down" => axis = Some(Axis::Vertical),
                        name if program.is_none()
                            && (AGENTS.iter().any(|a| a.1 == name)
                                || self.config.ai.contains_key(name)) =>
                        {
                            program = Some(name)
                        }
                        name if program.is_some() && model.is_none() => model = Some(name),
                        _ => bail!(
                            "Use :ai [codex|claude|gemini|agy|local|<your [ai.name]>] [model] [right|below]"
                        ),
                    }
                }
                match program {
                    Some(program) => {
                        let axis = axis.unwrap_or_else(|| self.ai_axis());
                        self.launch(program, model, Some(axis))?;
                    }
                    None => self.open_ai(axis)?,
                }
            }
            "theme" if !arg.is_empty() => {
                self.apply_theme(arg)?;
                self.theme_stamp = config::theme_stamp(arg);
                let origin = match (arg, config::theme_file(arg)) {
                    (crate::omarchy::FOLLOW, _) => format!(
                        " · follows Omarchy ({})",
                        crate::omarchy::current_name().unwrap_or_else(|| "current theme".into())
                    ),
                    (_, Some(path)) => format!(" · {}", path.display()),
                    (_, None) => " · built in".into(),
                };
                // The choice is remembered for the next launch.
                match config::save_theme(arg) {
                    Ok(_) => self.notice(format!("Theme: {arg}{origin} · saved as default")),
                    Err(error) => self.fail(format!("Theme: {arg} (not saved: {error:#})")),
                }
            }
            "theme" | "themes" => self.open_picker(),
            "set" => match arg {
                "rnu" | "relativenumber" => self.config.relative_numbers = true,
                "nornu" | "norelativenumber" => self.config.relative_numbers = false,
                _ => bail!("Supported: :set rnu / :set nornu"),
            },
            "files" => {
                self.sidebar_visible = true;
                self.sidebar_focus = true;
                self.git_focus = false;
            }
            "help" | "h" => self.toggle_help(),
            "bn" => self.next_buffer(false),
            "bp" => self.next_buffer(true),
            "run" => {
                let plugin = self.config.plugins.get(arg).cloned().with_context(|| {
                    format!(
                        "Unknown plugin: {arg}. Configure it in {}/config.toml",
                        config::config_dir().display()
                    )
                })?;
                self.open_terminal(Some((&plugin.program, plugin.args)), Some(Axis::Vertical))?;
                if let Some(Pane::Terminal(t)) = self.panes.get_mut(&self.focused) {
                    t.launch = Launch::Command { name: arg.into() };
                }
            }
            "" => {}
            _ => bail!("Unknown command: {verb}. F1 for help"),
        }
        Ok(())
    }
}

/// Keys that move the cursor in every mode (and extend a selection with Shift).
fn is_motion(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown
    )
}

/// Normal-mode motions that also extend a `v`/`V` selection.
fn is_vim_motion(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Char('h' | 'j' | 'k' | 'l' | 'w' | 'b' | '0' | '$' | '^' | 'G' | 'g' | '1'..='9')
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        app.event(Event::Key(KeyEvent::new(code, modifiers)));
    }
    #[test]
    fn keyboard_edit_save_search_jump_and_dirty_quit_guard() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        std::fs::write(&path, "first\nsecond\nthird\n").unwrap();
        let mut app = App::new(dir.path().into(), Some(path.clone()), false).unwrap();
        press(&mut app, KeyCode::Char('i'), KeyModifiers::NONE);
        app.event(Event::Paste("λ ".into()));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.quit(false).is_err());
        press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "λ first\nsecond\nthird\n"
        );
        app.command("3").unwrap();
        assert_eq!(app.active_buffer().unwrap().row, 2);
        app.active_buffer().unwrap().search = "second".into();
        press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(app.active_buffer().unwrap().row, 1);
        assert!(app.quit(false).is_ok());
    }
    #[test]
    fn copy_cut_paste_with_selection_lines_and_shift_arrows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.txt");
        std::fs::write(&path, "alpha beta\ngamma\n").unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        let text = |app: &mut App| app.active_buffer().unwrap().text.to_string();
        let none = KeyModifiers::NONE;
        // v + motion + y copies inclusively; P pastes at the cursor.
        press(&mut app, KeyCode::Char('v'), none);
        press(&mut app, KeyCode::Char('l'), none);
        press(&mut app, KeyCode::Char('l'), none);
        press(&mut app, KeyCode::Char('y'), none);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.clipboard.paste(), ("alp".into(), false));
        press(&mut app, KeyCode::Char('P'), none);
        assert_eq!(text(&mut app), "alpalpha beta\ngamma\n");
        // Ctrl-c without a selection copies the line; Ctrl-v pastes it above.
        press(&mut app, KeyCode::Char('u'), none);
        press(&mut app, KeyCode::Char('j'), none);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('v'), KeyModifiers::CONTROL);
        assert_eq!(text(&mut app), "alpha beta\ngamma\ngamma\n");
        // V + d cuts whole lines; p pastes them below.
        press(&mut app, KeyCode::Char('V'), none);
        press(&mut app, KeyCode::Char('d'), none);
        assert_eq!(text(&mut app), "alpha beta\ngamma\n");
        press(&mut app, KeyCode::Char('k'), none);
        press(&mut app, KeyCode::Char('p'), none);
        assert_eq!(text(&mut app), "alpha beta\ngamma\ngamma\n");
        // Shift+arrows in insert mode select exclusively; typing replaces the selection.
        press(&mut app, KeyCode::Char('g'), none);
        press(&mut app, KeyCode::Char('g'), none);
        press(&mut app, KeyCode::Char('i'), none);
        for _ in 0..5 {
            press(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        }
        assert_eq!(app.mode, Mode::Visual);
        press(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(app.mode, Mode::Insert);
        assert_eq!(app.clipboard.paste(), ("alpha".into(), false));
        press(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        press(&mut app, KeyCode::Char('Z'), KeyModifiers::SHIFT);
        assert_eq!(text(&mut app), "Zbeta\ngamma\ngamma\n");
        // Bracketed paste works without entering insert mode first.
        press(&mut app, KeyCode::Esc, none);
        app.event(Event::Paste("!".into()));
        assert!(text(&mut app).starts_with("!Zbeta") || text(&mut app).starts_with("Z!beta"));
    }
    #[cfg(unix)]
    #[test]
    fn lsp_diagnostics_completion_menu_and_format_through_the_app() {
        if std::process::Command::new("python3")
            .arg("-V")
            .output()
            .is_err()
        {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "fn main() {\n    x;\n}\n").unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        let mut servers = BTreeMap::new();
        servers.insert(
            "rust".to_string(),
            crate::config::CommandPlugin {
                program: "python3".into(),
                args: vec!["-c".into(), crate::lsp::FAKE_SERVER.into()],
            },
        );
        app.lsp = Lsp::new(dir.path().into(), true, servers);
        let settle = |app: &mut App, done: &dyn Fn(&App) -> bool| {
            for _ in 0..300 {
                app.tick();
                if done(app) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("timed out: {}", app.message);
        };
        let id = app.buffer_id().unwrap();
        settle(&mut app, &|a| a.lsp.diagnostics.contains_key(&id));
        // `.` is a trigger character: the menu opens, typing filters, Tab accepts.
        app.command("2").unwrap();
        press(&mut app, KeyCode::Char('A'), KeyModifiers::SHIFT);
        press(&mut app, KeyCode::Char('.'), KeyModifiers::NONE);
        settle(&mut app, &|a| a.menu.is_some());
        press(&mut app, KeyCode::Char('p'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(app.menu.is_none());
        assert_eq!(app.active_buffer().unwrap().line(1), "    x;.println!(x)");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        app.command("format").unwrap();
        settle(&mut app, &|a| a.message.starts_with("Formatted"));
        assert_eq!(app.active_buffer().unwrap().line(0), "pub fn main() {");
        app.command("lsp").unwrap();
        assert!(app.message.contains("python3 ready"), "{}", app.message);
    }
    #[test]
    fn theme_picker_previews_live_filters_and_restores_on_escape() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        app.command("theme nord").unwrap();
        // Ctrl-b t opens the picker on the current theme.
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::NONE);
        let picker = app.picker.as_ref().unwrap();
        assert_eq!(picker.themes[picker.shown[picker.selected]].0, "nord");
        assert!(picker.themes.iter().any(|(n, _)| n == "vscode-dark-2026"));
        // Moving previews the next theme immediately.
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_ne!(app.config.theme, "nord");
        // Esc restores the original.
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.picker.is_none());
        assert_eq!(app.config.theme, "nord");
        // Typing filters; Enter keeps the highlighted theme.
        app.command("themes").unwrap();
        for c in "drac".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(app.picker.as_ref().unwrap().shown.len(), 1);
        assert_eq!(app.config.theme, "dracula");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.picker.is_none());
        assert_eq!(app.theme.background, "#282a36");
        assert!(app.message.contains("saved as default"), "{}", app.message);
        // It renders at small and large sizes.
        app.command("themes").unwrap();
        for (width, height) in [(160, 45), (60, 16)] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
            let screen: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(screen.contains("THEMES"));
        }
    }
    #[test]
    fn browsing_the_explorer_previews_files_in_one_reused_buffer() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.path().join(name), format!("{name}\n")).unwrap();
        }
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        let shown = |app: &App| match app.panes.get(&0) {
            Some(Pane::Editor(id)) => app.buffers[*id].name(),
            _ => String::new(),
        };
        press(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert!(app.sidebar_focus);
        let start = app.buffers.len();
        // Each arrow press shows the highlighted file; focus stays in the sidebar.
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        let first = shown(&app);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        let second = shown(&app);
        assert_ne!(first, second);
        assert!(second.ends_with(".txt"));
        assert!(app.sidebar_focus);
        // Previews reuse one buffer instead of opening one per file.
        assert_eq!(app.buffers.len(), start + 1);
        // Enter keeps the file open and focuses it; the next preview gets a new buffer.
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(!app.sidebar_focus);
        assert_eq!(shown(&app), second);
        press(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(shown(&app), first);
        assert_eq!(app.buffers.len(), start + 2);
        assert!(app.buffers.iter().any(|b| b.name() == second));
        // An edited preview is kept, not replaced.
        let Some(Pane::Editor(id)) = app.panes.get(&0) else {
            panic!("editor pane")
        };
        let id = *id;
        app.buffers[id].insert("edit ");
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert!(app.buffers[id].dirty);
        assert_eq!(app.buffers[id].name(), first);
    }
    #[test]
    fn field_guide_stays_open_while_commands_and_edits_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hello\n").unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        assert!(app.help);
        // A command typed with the guide open runs, and the guide stays.
        press(&mut app, KeyCode::Char(':'), KeyModifiers::NONE);
        for c in "theme nord".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.config.theme, "nord");
        assert!(app.help);
        // Editing works too.
        press(&mut app, KeyCode::Char('A'), KeyModifiers::SHIFT);
        press(&mut app, KeyCode::Char('!'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.active_buffer().unwrap().line(0), "hello!");
        assert!(app.help);
        // Alt-arrows scroll the guide; it renders docked beside the panes.
        press(&mut app, KeyCode::Down, KeyModifiers::ALT);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("FIELD GUIDE"));
        assert!(screen.contains("a.txt"), "editor pane still visible");
        assert_eq!(app.help_scroll, 1);
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        assert!(!app.help);
    }
    #[test]
    fn relative_jumps_follow_the_numbers_shown_with_rnu() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lines.txt");
        let text: String = (1..=40).map(|i| format!("  line {i}\n")).collect();
        std::fs::write(&path, text).unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        app.command("set rnu").unwrap();
        app.command("20").unwrap();
        let row = |app: &mut App| app.active_buffer().unwrap().row;
        assert_eq!(row(&mut app), 19);
        // 15 then Enter: fifteen lines down, on the first non-blank.
        press(&mut app, KeyCode::Char('1'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('5'), KeyModifiers::NONE);
        assert!(app.message.contains("15 lines down"), "{}", app.message);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(row(&mut app), 34);
        assert_eq!(app.active_buffer().unwrap().col, 2);
        // 15 then -: fifteen lines up. 3k / 3j work too.
        press(&mut app, KeyCode::Char('1'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('5'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('-'), KeyModifiers::NONE);
        assert_eq!(row(&mut app), 19);
        press(&mut app, KeyCode::Char('3'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(row(&mut app), 16);
        // :+N / :-N in the command line; :N stays absolute.
        app.command("+10").unwrap();
        assert_eq!(row(&mut app), 26);
        app.command("-6").unwrap();
        assert_eq!(row(&mut app), 20);
        app.command("5").unwrap();
        assert_eq!(row(&mut app), 4);
    }
    #[test]
    fn ai_panes_open_beside_or_below_and_missing_clis_explain_install() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        let rects = |app: &mut App| {
            app.rectangles.clear();
            app.tiles
                .rectangles(Rect::new(0, 0, 120, 40), &mut app.rectangles);
            app.rectangles.clone()
        };
        app.command("ai below").unwrap();
        let r = rects(&mut app);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].1.x, r[1].1.x, "stacked panes share a column");
        app.close(false).unwrap();
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
        let r = rects(&mut app);
        assert_eq!(r[0].1.y, r[1].1.y, "side-by-side panes share a row");
        app.close(false).unwrap();
        // The config default applies when no placement is given.
        app.config.ai_split = "below".into();
        app.command("ai").unwrap();
        let r = rects(&mut app);
        assert_eq!(r[0].1.x, r[1].1.x);
        // A CLI that is not installed explains how to get it.
        assert!(matches!(app.panes.get(&app.focused), Some(Pane::Ai)));
        let error = app.launch_agent("odin-no-such-agent", None).unwrap_err();
        assert!(error.to_string().contains("not installed"));
        assert!(app.command("ai nonsense").is_err());
    }
    #[test]
    fn field_guide_fits_small_screens_and_scrolls_to_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        for (width, height) in [(80u16, 24u16), (60, 20), (120, 30)] {
            let mut seen = String::new();
            app.help_scroll = 0;
            let mut last = usize::MAX;
            while last != app.help_scroll {
                last = app.help_scroll;
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .unwrap();
                terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
                let screen: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                seen.push_str(&screen);
                press(&mut app, KeyCode::Down, KeyModifiers::ALT);
            }
            // Every section, down to the last row, is reachable at this size.
            for heading in [
                "THEMES",
                "WORKSPACE",
                "EDITOR",
                "MARKDOWN",
                "COMMANDS",
                "rnu",
            ] {
                assert!(seen.contains(heading), "{heading} at {width}x{height}");
            }
        }
    }
    #[cfg(unix)]
    #[test]
    fn your_own_ai_launchers_from_config_run_in_ai_panes() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        // Exactly what a user writes in config.toml.
        let config: crate::config::Config = toml::from_str(
            r#"
[ai.local]
program = "sh"
args = ["-c", "echo local {model}; sleep 5"]
model = "tiny"

[ai.llamacpp]
program = "sh"
args = ["-c", "echo $OPENAI_BASE_URL; sleep 5"]
env = { OPENAI_BASE_URL = "http://localhost:8080/v1" }
"#,
        )
        .unwrap();
        app.config.ai = config.ai;
        assert_eq!(app.custom_agents(), vec!["llamacpp"]);
        let terminals = |app: &App| {
            app.panes
                .values()
                .filter(|p| matches!(p, Pane::Terminal(_)))
                .count()
        };
        // :ai local uses [ai.local], with a model override.
        app.command("ai local big below").unwrap();
        assert_eq!(terminals(&app), 1);
        // Launcher key 6 runs the first custom entry in the AI pane itself.
        app.command("ai").unwrap();
        assert!(matches!(app.panes.get(&app.focused), Some(Pane::Ai)));
        press(&mut app, KeyCode::Char('6'), KeyModifiers::NONE);
        assert_eq!(terminals(&app), 2);
        assert!(matches!(app.panes.get(&app.focused), Some(Pane::Terminal(t)) if t.title == "sh"));
        // Unknown names and missing programs explain themselves.
        assert!(app.command("ai nosuch").is_err());
        app.config.ai.get_mut("llamacpp").unwrap().program = "odin-no-such-llm".into();
        let error = app.command("ai llamacpp").unwrap_err().to_string();
        assert!(error.contains("not on PATH"), "{error}");
    }
    #[test]
    fn pane_shortcuts_focus_and_collapse_without_losing_buffers() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('%'), KeyModifiers::NONE);
        assert_eq!(app.panes.len(), 2);
        app.tiles
            .rectangles(Rect::new(0, 0, 120, 40), &mut app.rectangles);
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(app.focused, 0);
        app.close(false).unwrap();
        assert_eq!(app.panes.len(), 1);
        assert!(app.tiles.contains(app.focused));
    }
    #[test]
    fn rendering_contains_editor_tree_git_and_ai_at_multiple_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "fn main() { println!(\"hello\"); }\n").unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        app.add_pane(Pane::Ai, Axis::Horizontal);
        for (width, height) in [(160, 45), (80, 24), (45, 12), (20, 8)] {
            let backend = ratatui::backend::TestBackend::new(width, height);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
            let rendered = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(rendered.contains("ODIN"));
            if width == 160 {
                for needle in [
                    "EXPLORER",
                    "GIT / TREE",
                    "main.rs",
                    "OpenAI Codex",
                    "Claude Code",
                ] {
                    assert!(rendered.contains(needle), "Missing {needle}");
                }
            }
        }
    }
    #[test]
    fn missing_ai_binary_reports_error_without_losing_pane() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        assert!(app
            .open_terminal(
                Some(("/nonexistent/odin-test-cli", Vec::new())),
                Some(Axis::Horizontal)
            )
            .is_err());
        assert_eq!(app.panes.len(), 1);
    }
    #[test]
    fn switching_all_themes_rebuilds_syntax_and_preserves_unsaved_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "fn main() { let number = 42; }\n").unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        let id = app.buffer_id().unwrap();
        press(&mut app, KeyCode::Char('i'), KeyModifiers::NONE);
        app.event(Event::Paste("// unsaved\n".into()));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        let original = app.buffers[id].text.to_string();
        assert!(app.buffers[id].dirty);
        for (name, _) in config::BUNDLED_THEMES {
            app.syntax.prepare(id, &app.buffers[id], 10);
            assert!(!app.syntax.cache.is_empty());
            app.command(&format!("theme {name}")).unwrap();
            assert!(app.syntax.cache.is_empty());
            assert_eq!(app.config.theme, *name);
            let backend = ratatui::backend::TestBackend::new(100, 30);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
            assert_eq!(
                terminal.backend().buffer()[(99, 10)].bg,
                config::color(&app.theme.background)
            );
            assert_eq!(app.buffers[id].text.to_string(), original);
            assert!(app.buffers[id].dirty);
        }
        assert!(app.command("theme ../../bad").is_err());
        assert_eq!(app.config.theme, "github-dark");
    }
    #[test]
    fn git_pane_shows_every_branch_and_maximizes() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(["-c", "user.name=Ada", "-c", "user.email=ada@example.com"])
                .args([
                    "-c",
                    "init.defaultBranch=main",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["commit", "-q", "--allow-empty", "-m", "first"]);
        git(&["checkout", "-q", "-b", "feature"]);
        git(&["commit", "-q", "--allow-empty", "-m", "feature work"]);
        git(&["checkout", "-q", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "main work"]);
        git(&["merge", "-q", "--no-ff", "-m", "merge feature", "feature"]);
        git(&["tag", "v1.0"]);
        git(&["checkout", "-q", "-b", "topic", "HEAD~1"]);
        git(&["commit", "-q", "--allow-empty", "-m", "unmerged idea"]);
        git(&["checkout", "-q", "main"]);
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        app.command("git").unwrap();
        let git_id = app.focused;
        assert!(matches!(app.panes.get(&git_id), Some(Pane::Git(_))));
        for _ in 0..200 {
            app.tick();
            if let Some(Pane::Git(view)) = app.panes.get(&git_id) {
                if view.data.commits.len() == 5 && view.detail.is_some() {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 40)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .map(|y| {
                let row: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect();
                row.trim_end().to_string() + "\n"
            })
            .collect();
        for expected in [
            "feature",
            "topic",
            "merge feature",
            "unmerged idea",
            "v1.0",
            "◉ main",
        ] {
            assert!(screen.contains(expected), "missing {expected}");
        }
        // Selecting a branch jumps to its newest commit; Enter shows only it.
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        let Some(Pane::Git(view)) = app.panes.get(&git_id) else {
            unreachable!()
        };
        let topic = view
            .data
            .branches
            .iter()
            .position(|b| b.name == "topic")
            .unwrap();
        for _ in 0..topic {
            press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        }
        let Some(Pane::Git(view)) = app.panes.get(&git_id) else {
            unreachable!()
        };
        assert_eq!(view.selected().unwrap().subject, "unmerged idea");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let Some(Pane::Git(view)) = app.panes.get(&git_id) else {
            unreachable!()
        };
        assert_eq!(view.only.as_deref(), Some("topic"));
        // Ctrl-b G maximizes and restores the focused Git pane, and refocuses it.
        let prefix_g = |app: &mut App| {
            press(app, KeyCode::Char('b'), KeyModifiers::CONTROL);
            press(app, KeyCode::Char('G'), KeyModifiers::SHIFT);
        };
        prefix_g(&mut app);
        assert!(app.zoom);
        // Maximized, the selected commit's details have room.
        for _ in 0..200 {
            app.tick();
            if let Some(Pane::Git(view)) = app.panes.get(&git_id) {
                if view.data.commits.len() == 4 && view.detail.is_some() {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = buffer.content.iter().map(|c| c.symbol()).collect();
        assert!(
            screen.contains("Ada <ada@example.com>  20"),
            "commit details shown"
        );
        prefix_g(&mut app);
        assert!(!app.zoom);
        app.focused = 0;
        prefix_g(&mut app);
        assert_eq!(app.focused, git_id);
        assert_eq!(app.panes.len(), 2, "one Git pane, reused");
    }
    #[cfg(unix)]
    #[test]
    fn sessions_bring_back_panes_unsaved_text_and_terminal_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "saved line\n").unwrap();
        let mut app = App::new(dir.path().into(), Some(path.clone()), false).unwrap();
        press(&mut app, KeyCode::Char('A'), KeyModifiers::SHIFT);
        for c in " + unsaved".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        app.open_terminal(
            Some((
                "/bin/sh",
                vec!["-c".into(), "cd sub; echo shell-marker; exec sh".into()],
            )),
            Some(Axis::Vertical),
        )
        .unwrap();
        let shell = app.focused;
        for _ in 0..200 {
            app.tick();
            let Some(Pane::Terminal(t)) = app.panes.get(&shell) else {
                unreachable!()
            };
            if t.transcript().contains("shell-marker") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        app.focused = 0;
        app.save_session(false).unwrap();
        let mut leaves = Vec::new();
        app.tiles.leaves(&mut leaves);
        drop(app);

        let mut app = App::new(dir.path().into(), None, false).unwrap();
        app.restore_session();
        assert!(
            app.message.starts_with("Session restored"),
            "{}",
            app.message
        );
        let mut restored = Vec::new();
        app.tiles.leaves(&mut restored);
        assert_eq!(restored, leaves);
        assert_eq!(app.focused, 0);
        let buffer = app.active_buffer().unwrap();
        assert_eq!(buffer.line(0), "saved line + unsaved");
        assert!(buffer.dirty, "unsaved text stays unsaved");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "saved line\n");
        let Some(Pane::Terminal(t)) = app.panes.get(&shell) else {
            panic!("terminal pane restored")
        };
        assert!(t.transcript().contains("shell-marker"));
        assert!(t.transcript().contains("restored from your last session"));
        // The shell came back in the directory it had moved to.
        let mut cwd = None;
        for _ in 0..100 {
            cwd = t.cwd();
            if cwd.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Some(cwd) = cwd {
            assert!(cwd.ends_with("sub"), "{}", cwd.display());
        }
        let ignore = std::fs::read_to_string(dir.path().join(".odin/.gitignore")).unwrap();
        assert!(ignore.contains('*'));
    }
}
