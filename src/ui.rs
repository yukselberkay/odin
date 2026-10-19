use crate::{
    app::{App, InputKind, Mode, Pane},
    config::{color, Theme},
    editor::Buffer,
    gitgraph::{self, Focus as GitFocus, GitView, RefKind},
    lsp::Diagnostic,
    markdown,
    workspace::printable,
};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthChar;

fn panel<'a>(title: impl Into<Line<'a>>, theme: &Theme, active: bool) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color(if active {
            &theme.accent
        } else {
            &theme.border
        })))
        .title(title)
        .title_style(Style::default().fg(color(if active { &theme.accent } else { &theme.muted })))
}
fn text(frame: &mut Frame, area: Rect, value: impl Into<String>, fg: Color) {
    if area.height > 0 && area.width > 0 {
        frame.render_widget(
            Paragraph::new(value.into()).style(Style::default().fg(fg)),
            area,
        );
    }
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let theme = app.theme.clone();
    frame.render_widget(
        Block::default().style(
            Style::default()
                .bg(color(&theme.background))
                .fg(color(&theme.foreground)),
        ),
        area,
    );
    if area.width < 45 || area.height < 12 {
        frame.render_widget(
            Paragraph::new(
                "ODIN\nTerminal too small.\nResize to at least 45 × 12.\nCtrl-q to quit.",
            )
            .alignment(Alignment::Center),
            area,
        );
        return;
    }
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(area);
    let root = app.root.file_name().unwrap_or_default().to_string_lossy();
    let title = Line::from(vec![
        Span::styled(
            " ◉ ODIN ",
            Style::default()
                .fg(color(&theme.background))
                .bg(color(&theme.accent))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", printable(&root)),
            Style::default().fg(color(&theme.muted)),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(title),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if area.width > 110 {
        let right = format!(
            "{}  ·  {} panes  ·  v0.1.0 ",
            if app.git.branch.is_empty() {
                "local"
            } else {
                &app.git.branch
            },
            app.panes.len()
        );
        frame.render_widget(
            Paragraph::new(right)
                .style(Style::default().fg(color(&theme.muted)))
                .alignment(Alignment::Right),
            Rect::new(area.x + area.width - 45, area.y, 45, 1),
        );
    }
    let body = layout[1];
    // On narrower screens the open guide takes the sidebar's room.
    let sidebar_shown = app.sidebar_visible && area.width >= 70 && !(app.help && area.width < 140);
    let content = if sidebar_shown {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(app.config.sidebar_width),
                Constraint::Min(25),
            ])
            .split(body);
        sidebar(frame, app, columns[0]);
        columns[1]
    } else {
        body
    };
    // The guide docks on the right so the panes and command line stay usable.
    let content = if app.help {
        let width = if content.width >= 110 {
            (content.width * 45 / 100).clamp(56, 86)
        } else if content.width >= 70 {
            // Keep ~30 columns for your work and give the guide the rest, up to 56.
            (content.width / 2).max(content.width.saturating_sub(30).min(56))
        } else {
            // Too small to share: the guide covers the panes; F1 gives them back.
            content.width
        };
        let guide = Rect::new(content.right() - width, content.y, width, content.height);
        help(frame, app, guide);
        Rect::new(content.x, content.y, content.width - width, content.height)
    } else {
        content
    };
    app.rectangles.clear();
    if app.zoom {
        app.rectangles.push((app.focused, content));
    } else {
        app.tiles.rectangles(content, &mut app.rectangles);
    }
    let rectangles = app.rectangles.clone();
    let selection = app.selection();
    let customs = app.custom_agents();
    for (id, rect) in rectangles {
        let active = id == app.focused && !app.sidebar_focus;
        let title = match app.panes.get(&id) {
            Some(Pane::Editor(buffer)) => format!(
                " {} {}{} ",
                id + 1,
                printable(&app.buffers[*buffer].name()),
                if app.buffers[*buffer].dirty {
                    " ●"
                } else {
                    ""
                }
            ),
            Some(Pane::Preview(p)) => format!(
                " {} {} · preview{} ",
                id + 1,
                printable(&app.buffers[p.buffer].name()),
                if app.buffers[p.buffer].dirty {
                    " ●"
                } else {
                    ""
                }
            ),
            Some(Pane::Terminal(t)) => format!(
                " {} {}{} ",
                id + 1,
                t.title,
                if t.exited.is_some() {
                    " · exited"
                } else {
                    " · live"
                }
            ),
            Some(Pane::Git(view)) => format!(
                " {} GIT · {} ",
                id + 1,
                match &view.only {
                    Some(name) => format!("{} only · a shows all", printable(name)),
                    None => "all branches".into(),
                }
            ),
            _ => format!(" {} AI / copilot ", id + 1),
        };
        let block = panel(title, &theme, active);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width < 2 || inner.height < 1 {
            continue;
        }
        match app.panes.get_mut(&id) {
            Some(Pane::Editor(buffer_id)) => {
                let buffer_id = *buffer_id;
                let buffer = &mut app.buffers[buffer_id];
                let footer = inner.height > 3;
                let height = inner.height - u16::from(footer);
                if buffer.row < buffer.scroll {
                    buffer.scroll = buffer.row;
                }
                if buffer.row >= buffer.scroll + height as usize {
                    buffer.scroll = buffer.row.saturating_sub(height.saturating_sub(1) as usize);
                }
                app.redraw |=
                    app.syntax
                        .prepare(buffer_id, buffer, buffer.scroll + height as usize);
                let lines = app.syntax.cache.get(&buffer_id).map(|c| &c.lines);
                let selection = if active { selection } else { None };
                let diagnostics = app
                    .lsp
                    .diagnostics
                    .get(&buffer_id)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let cursor = editor(
                    frame,
                    buffer,
                    lines,
                    selection,
                    diagnostics,
                    Rect::new(inner.x, inner.y, inner.width, height),
                    &theme,
                    active && app.input.is_none(),
                    app.config.relative_numbers,
                    app.config.tab_width,
                );
                if active {
                    app.cursor_at = cursor;
                }
                if footer {
                    let area = Rect::new(inner.x, inner.bottom() - 1, inner.width, 1);
                    // The diagnostic under the cursor line replaces the status details.
                    if let Some(d) = diagnostics.iter().find(|d| d.row == buffer.row) {
                        text(
                            frame,
                            area,
                            format!(" ● {}", printable(&d.message)),
                            severity_color(&theme, d.severity),
                        );
                    } else {
                        let language = app.syntax.language(buffer);
                        let errors = diagnostics.iter().filter(|d| d.severity == 1).count();
                        let warnings = diagnostics.iter().filter(|d| d.severity == 2).count();
                        let mut server = app
                            .lsp
                            .status(buffer_id, buffer)
                            .map(|s| format!("  ·  {s}"))
                            .unwrap_or_default();
                        if errors + warnings > 0 {
                            server.push_str(&format!("  ·  {errors}E {warnings}W"));
                        }
                        let detail = format!(
                            " {language}{server}  ·  UTF-8 / {}     {}:{}  {}%",
                            if buffer.crlf { "CRLF" } else { "LF" },
                            buffer.row + 1,
                            buffer.col + 1,
                            (buffer.row + 1) * 100 / buffer.lines()
                        );
                        text(frame, area, detail, color(&theme.muted));
                    }
                }
            }
            Some(Pane::Terminal(terminal)) => {
                let _ = terminal.resize(inner.height, inner.width);
                if let Ok(parser) = terminal.parser.lock() {
                    let screen = parser.screen();
                    for row in 0..inner.height {
                        for col in 0..inner.width {
                            if let Some(cell) = screen.cell(row, col) {
                                if cell.is_wide_continuation() {
                                    continue;
                                }
                                let mut style = Style::default()
                                    .fg(term_color(
                                        cell.fgcolor(),
                                        color(&theme.foreground),
                                        &theme,
                                    ))
                                    .bg(term_color(
                                        cell.bgcolor(),
                                        color(&theme.background),
                                        &theme,
                                    ));
                                if cell.bold() {
                                    style = style.add_modifier(Modifier::BOLD);
                                }
                                if cell.italic() {
                                    style = style.add_modifier(Modifier::ITALIC);
                                }
                                if cell.underline() {
                                    style = style.add_modifier(Modifier::UNDERLINED);
                                }
                                if cell.inverse() {
                                    style = style.add_modifier(Modifier::REVERSED);
                                }
                                let contents = cell.contents();
                                let symbol = if contents.is_empty() { " " } else { &contents };
                                let x = inner.x + col;
                                let y = inner.y + row;
                                frame.buffer_mut().set_stringn(
                                    x,
                                    y,
                                    symbol,
                                    (inner.width - col) as usize,
                                    style,
                                );
                            }
                        }
                    }
                    if active
                        && app.input.is_none()
                        && !screen.hide_cursor()
                        && screen.scrollback() == 0
                        && terminal.exited.is_none()
                    {
                        let (row, col) = screen.cursor_position();
                        if row < inner.height && col < inner.width {
                            frame.set_cursor_position((inner.x + col, inner.y + row));
                        }
                    }
                }
                if let Some(exit) = &terminal.exited {
                    text(
                        frame,
                        Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
                        format!("{exit} · Ctrl-b x closes pane"),
                        color(&theme.warning),
                    );
                }
            }
            Some(Pane::Preview(preview)) => {
                let buffer = &app.buffers[preview.buffer];
                let footer = inner.height > 3;
                let height = (inner.height - u16::from(footer)) as usize;
                // One column for the scrollbar, at least two of margin per side.
                let room = (inner.width as usize).saturating_sub(5);
                let width = room.clamp(1, markdown::MAX_WIDTH);
                let left = inner.x + 2 + ((room - width) / 2) as u16;
                let doc = app
                    .previews
                    .document(preview.buffer, buffer, width, &theme, &app.syntax);
                // Follow the source cursor (live split preview, or returning from the editor).
                if preview.synced != Some(buffer.row) {
                    preview.synced = Some(buffer.row);
                    preview.scroll = doc.line_for_source(buffer.row).saturating_sub(1);
                }
                let max = doc.lines.len().saturating_sub(height);
                preview.scroll = preview.scroll.min(max);
                preview.height = height;
                for (offset, line) in doc
                    .lines
                    .iter()
                    .skip(preview.scroll)
                    .take(height)
                    .enumerate()
                {
                    frame.render_widget(
                        Paragraph::new(line.clone()),
                        Rect::new(left, inner.y + offset as u16, width as u16, 1),
                    );
                }
                if doc.lines.is_empty() {
                    text(
                        frame,
                        Rect::new(left, inner.y, width as u16, 1),
                        "Empty document — press e to write",
                        color(&theme.muted),
                    );
                }
                if max > 0 && height > 1 {
                    let x = inner.right() - 1;
                    let thumb = (height * height / doc.lines.len()).max(1);
                    let at = preview.scroll * (height - thumb) / max;
                    for row in 0..height {
                        let on = (at..at + thumb).contains(&row);
                        text(
                            frame,
                            Rect::new(x, inner.y + row as u16, 1, 1),
                            if on { "┃" } else { "│" },
                            color(if on { &theme.accent } else { &theme.border }),
                        );
                    }
                }
                if footer {
                    let percent = (preview.scroll * 100).checked_div(max).unwrap_or(100);
                    let detail = format!(
                        " Markdown preview  ·  {} lines  ·  e edit  ·  [ ] headings     {percent}%",
                        buffer.lines()
                    );
                    text(
                        frame,
                        Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
                        detail,
                        color(&theme.muted),
                    );
                }
            }
            Some(Pane::Ai) => ai(frame, inner, &theme, &customs),
            Some(Pane::Git(view)) => git_view(frame, view, inner, &theme),
            None => {}
        }
    }
    if app.input.is_none() {
        if let Some(at) = app.cursor_at.filter(|_| app.buffer_id().is_some()) {
            if app.menu.is_some() {
                completion_menu(frame, app, at, area);
            } else if let Some(hover) = &app.hover {
                hover_popup(frame, app, hover, at, area);
            }
        }
    }
    footer(frame, app, layout[2]);
    if app.picker.is_some() {
        theme_picker(frame, app, area);
    }
}

fn theme_picker(frame: &mut Frame, app: &App, area: Rect) {
    let Some(picker) = &app.picker else {
        return;
    };
    let theme = &app.theme;
    let width = area.width.saturating_sub(6).min(52);
    let rows = picker.shown.len().clamp(1, 16);
    let height = (rows as u16 + 4).min(area.height.saturating_sub(4));
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let block = panel(
        format!(
            " THEMES · {} · Enter keep · Esc cancel ",
            picker.shown.len()
        ),
        theme,
        true,
    )
    .style(Style::default().bg(color(&theme.background)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.height < 2 {
        return;
    }
    let filter = if picker.filter.is_empty() {
        Span::styled(" type to filter…", Style::default().fg(color(&theme.muted)))
    } else {
        Span::styled(
            format!(" filter: {}", printable(&picker.filter)),
            Style::default().fg(color(&theme.accent)),
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(filter)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let visible = (inner.height - 1) as usize;
    let top = picker.selected.saturating_sub(visible.saturating_sub(1));
    let chosen = crate::markdown::mix(color(&theme.selection), color(&theme.accent), 0.45);
    if picker.shown.is_empty() {
        text(
            frame,
            Rect::new(inner.x + 1, inner.y + 1, inner.width.saturating_sub(1), 1),
            "No theme matches",
            color(&theme.muted),
        );
    }
    for (offset, &index) in picker.shown.iter().skip(top).take(visible).enumerate() {
        let (name, origin) = &picker.themes[index];
        let selected = top + offset == picker.selected;
        let mark = if *name == picker.original { "●" } else { " " };
        let base = if selected {
            Style::default().bg(chosen)
        } else {
            Style::default()
        };
        let name_width = (inner.width as usize).saturating_sub(15);
        let line = Line::from(vec![
            Span::styled(format!(" {mark} "), base.fg(color(&theme.accent))),
            Span::styled(
                format!("{:<name_width$}", printable(name)),
                base.fg(color(&theme.foreground)),
            ),
            Span::styled(
                format!("{origin:>9} "),
                base.fg(color(if selected {
                    &theme.foreground
                } else {
                    &theme.muted
                })),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(inner.x, inner.y + 1 + offset as u16, inner.width, 1),
        );
    }
}

fn sidebar(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = &app.theme;
    let git_height = (area.height / 3).clamp(8, 16).min(area.height / 2);
    let regions = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(git_height)])
        .split(area);
    let block = panel(" EXPLORER ", theme, app.sidebar_focus && !app.git_focus);
    let inner = block.inner(regions[0]);
    frame.render_widget(block, regions[0]);
    let name = app.root.file_name().unwrap_or_default().to_string_lossy();
    text(
        frame,
        Rect::new(inner.x, inner.y, inner.width, 1),
        format!(" {} /", printable(&name)),
        color(&theme.accent),
    );
    let visible = inner.height.saturating_sub(3) as usize;
    if app.tree.selected < app.tree.offset {
        app.tree.offset = app.tree.selected;
    }
    if visible > 0 && app.tree.selected >= app.tree.offset + visible {
        app.tree.offset = app.tree.selected + 1 - visible;
    }
    for (offset, (index, entry)) in app
        .tree
        .entries
        .iter()
        .enumerate()
        .skip(app.tree.offset)
        .take(visible)
        .enumerate()
    {
        let name = entry.path.file_name().unwrap_or_default().to_string_lossy();
        let marker = if entry.directory {
            if app.tree.expanded.contains(&entry.path) {
                "▾"
            } else {
                "▸"
            }
        } else if entry.symlink {
            "↗"
        } else {
            "·"
        };
        let relative = entry
            .path
            .strip_prefix(&app.root)
            .ok()
            .map(|p| p.to_string_lossy());
        let changed = relative
            .as_ref()
            .and_then(|path| app.git.changes.iter().find(|(_, p)| p == path.as_ref()))
            .map(|(code, _)| code.trim());
        let mut style = Style::default().fg(color(if entry.directory {
            &theme.secondary
        } else {
            &theme.foreground
        }));
        if index == app.tree.selected {
            style = style.bg(color(&theme.selection));
            if app.sidebar_focus && !app.git_focus {
                style = style.fg(color(&theme.accent)).add_modifier(Modifier::BOLD);
            }
        }
        let suffix = changed.map(|c| format!(" {c}")).unwrap_or_default();
        let indent = "  ".repeat(entry.depth);
        let line = if app.config.icons != crate::config::Icons::Off {
            let open = app.tree.expanded.contains(&entry.path);
            let icon = if entry.directory {
                crate::icons::folder(open, &name)
            } else {
                crate::icons::file(&entry.path)
            };
            let (glyph, tint) = if app.config.icons == crate::config::Icons::Basic {
                crate::icons::basic(icon, entry.directory, open)
            } else {
                icon
            };
            let folder_tint = entry.directory.then(|| color(&theme.secondary));
            let icon_style = match tint.or(folder_tint) {
                Some(tint) => style.fg(tint),
                None => style,
            };
            let arrow = if entry.directory {
                marker
            } else if entry.symlink {
                "↗"
            } else {
                " "
            };
            Line::from(vec![
                Span::styled(format!(" {indent}{arrow} "), style),
                Span::styled(format!("{glyph} "), icon_style),
                Span::styled(format!("{}{suffix}", printable(&name)), style),
            ])
        } else {
            Line::styled(
                format!(" {indent}{marker} {}{suffix}", printable(&name)),
                style,
            )
        };
        frame.render_widget(
            Paragraph::new(line).style(style),
            Rect::new(inner.x, inner.y + 1 + offset as u16, inner.width, 1),
        );
    }
    if inner.height > 3 {
        text(
            frame,
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            " ↑↓ move  → open  r refresh",
            color(&theme.muted),
        );
    }
    let block = panel(" GIT / TREE ", theme, app.sidebar_focus && app.git_focus);
    let inner = block.inner(regions[1]);
    frame.render_widget(block, regions[1]);
    let mut lines = Vec::new();
    if let Some(error) = &app.git.error {
        lines.push(Line::styled(
            format!(" {error}"),
            Style::default().fg(color(&theme.muted)),
        ));
    } else {
        lines.push(Line::styled(
            format!(" ⎇ {}", app.git.branch),
            Style::default()
                .fg(color(&theme.accent))
                .add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::styled(
            format!(
                " {} changed · {}",
                app.git.changes.len(),
                if app.git.changes.is_empty() {
                    "clean"
                } else {
                    "working tree"
                }
            ),
            Style::default().fg(color(&theme.muted)),
        ));
        let max_changes = if app.git.graph.is_empty() {
            inner.height.saturating_sub(3) as usize
        } else {
            (inner.height / 3).max(1) as usize
        };
        let start = if app.git_focus {
            app.git_selected
                .saturating_sub(max_changes.saturating_sub(1))
        } else {
            0
        };
        for (index, (code, path)) in app
            .git
            .changes
            .iter()
            .enumerate()
            .skip(start)
            .take(max_changes)
        {
            let mut style = Style::default().fg(color(&theme.warning));
            if app.sidebar_focus && app.git_focus && index == app.git_selected {
                style = style.bg(color(&theme.selection));
            }
            lines.push(Line::styled(format!(" {code} {}", printable(path)), style));
        }
        if !app.git.graph.is_empty() {
            lines.push(Line::from(""));
        }
        for graph in &app.git.graph {
            lines.push(Line::styled(
                format!(" {graph}"),
                Style::default().fg(color(&theme.secondary)),
            ));
        }
        if app.git.graph.is_empty() && app.git.changes.is_empty() {
            lines.push(Line::styled(
                " No commits yet",
                Style::default().fg(color(&theme.muted)),
            ));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

#[allow(clippy::too_many_arguments)]
fn editor(
    frame: &mut Frame,
    buffer: &mut Buffer,
    highlight: Option<&Vec<Vec<(syntect::highlighting::Style, String)>>>,
    selection: Option<(usize, usize)>,
    diagnostics: &[Diagnostic],
    area: Rect,
    theme: &Theme,
    active: bool,
    relative: bool,
    tab_width: usize,
) -> Option<(u16, u16)> {
    let digits = buffer.lines().to_string().len().max(3);
    let gutter = (digits + 2) as u16;
    if area.width <= gutter {
        return None;
    }
    let width = area.width - gutter;
    let current = buffer.line(buffer.row);
    let cursor_x = display_width(current.chars().take(buffer.col), tab_width);
    if cursor_x < buffer.hscroll {
        buffer.hscroll = cursor_x;
    }
    if cursor_x >= buffer.hscroll + width as usize {
        buffer.hscroll = cursor_x + 1 - width as usize;
    }
    for offset in 0..area.height {
        let row = buffer.scroll + offset as usize;
        let y = area.y + offset;
        if row >= buffer.lines() {
            text(
                frame,
                Rect::new(area.x, y, gutter, 1),
                "  ~",
                color(&theme.border),
            );
            continue;
        }
        let current_row = row == buffer.row;
        if current_row {
            frame.render_widget(
                Block::default().style(Style::default().bg(color(&theme.selection))),
                Rect::new(area.x, y, area.width, 1),
            );
        }
        let number = if relative && !current_row {
            row.abs_diff(buffer.row)
        } else {
            row + 1
        };
        text(
            frame,
            Rect::new(area.x, y, gutter, 1),
            format!("{number:>digits$} {}", if current_row { "▎" } else { "│" }),
            color(if current_row {
                &theme.accent
            } else {
                theme.line_number.as_ref().unwrap_or(&theme.muted)
            }),
        );
        let marks: Vec<&Diagnostic> = diagnostics.iter().filter(|d| d.row == row).collect();
        if let Some(worst) = marks.iter().map(|d| d.severity).min() {
            text(
                frame,
                Rect::new(area.x + digits as u16 + 1, y, 1, 1),
                "●",
                severity_color(theme, worst),
            );
        }
        let raw = buffer.line(row);
        let regions = highlight
            .and_then(|lines| lines.get(row))
            .filter(|line| !line.is_empty());
        let mut spans = Vec::new();
        let mut column = 0;
        if let Some(regions) = regions {
            for (style, value) in regions {
                let fg = style.foreground;
                let mut rs = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
                if style
                    .font_style
                    .contains(syntect::highlighting::FontStyle::BOLD)
                {
                    rs = rs.add_modifier(Modifier::BOLD);
                }
                if style
                    .font_style
                    .contains(syntect::highlighting::FontStyle::ITALIC)
                {
                    rs = rs.add_modifier(Modifier::ITALIC);
                }
                let clipped = clip(
                    value,
                    &mut column,
                    buffer.hscroll,
                    width as usize,
                    tab_width,
                );
                if !clipped.is_empty() {
                    spans.push(Span::styled(clipped, rs));
                }
            }
        } else {
            spans.push(Span::raw(clip(
                &raw,
                &mut column,
                buffer.hscroll,
                width as usize,
                tab_width,
            )));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(area.x + gutter, y, width, 1),
        );
        for d in &marks {
            let x0 = display_width(raw.chars().take(d.start), tab_width);
            let x1 = display_width(raw.chars().take(d.end), tab_width).max(x0 + 1);
            let style = Style::default()
                .add_modifier(Modifier::UNDERLINED)
                .underline_color(severity_color(theme, d.severity));
            for col in x0.max(buffer.hscroll)..x1.min(buffer.hscroll + width as usize) {
                if let Some(cell) = frame
                    .buffer_mut()
                    .cell_mut((area.x + gutter + (col - buffer.hscroll) as u16, y))
                {
                    cell.set_style(style);
                }
            }
        }
        if let Some((start, end)) = selection {
            let line_start = buffer.text.line_to_char(row);
            let line_end = line_start + raw.chars().count();
            if start <= line_end && end > line_start {
                let from = start.max(line_start) - line_start;
                let to = end.min(line_end) - line_start;
                let x0 = display_width(raw.chars().take(from), tab_width);
                // A selection running past the line end also covers the newline cell.
                let x1 =
                    display_width(raw.chars().take(to), tab_width) + usize::from(end > line_end);
                let bg = crate::markdown::mix(color(&theme.selection), color(&theme.accent), 0.45);
                for col in x0.max(buffer.hscroll)..x1.min(buffer.hscroll + width as usize) {
                    if let Some(cell) = frame
                        .buffer_mut()
                        .cell_mut((area.x + gutter + (col - buffer.hscroll) as u16, y))
                    {
                        cell.set_bg(bg);
                    }
                }
            }
        }
        // Search marks are rendered after syntax, but never alter the source.
        if !buffer.search.is_empty() {
            for (byte, matched) in raw.match_indices(&buffer.search) {
                let x = display_width(raw[..byte].chars(), tab_width);
                let len = display_width(matched.chars(), tab_width);
                let from = x.max(buffer.hscroll);
                let to = (x + len).min(buffer.hscroll + width as usize);
                for col in from..to {
                    if let Some(cell) = frame
                        .buffer_mut()
                        .cell_mut((area.x + gutter + (col - buffer.hscroll) as u16, y))
                    {
                        cell.set_bg(color(&theme.warning))
                            .set_fg(color(&theme.background));
                    }
                }
            }
        }
    }
    if active && buffer.row >= buffer.scroll && buffer.row < buffer.scroll + area.height as usize {
        let at = (
            area.x + gutter + (cursor_x.saturating_sub(buffer.hscroll) as u16).min(width - 1),
            area.y + (buffer.row - buffer.scroll) as u16,
        );
        frame.set_cursor_position(at);
        return Some(at);
    }
    None
}

fn severity_color(theme: &Theme, severity: u8) -> Color {
    match severity {
        1 => theme
            .ansi
            .as_ref()
            .map(|a| color(&a[9]))
            .unwrap_or_else(|| color(&theme.warning)),
        2 => color(&theme.warning),
        3 => color(&theme.secondary),
        _ => color(&theme.muted),
    }
}

/// Place a popup of `width`×`height` below the cursor, or above it when there is no room.
fn popup_rect(at: (u16, u16), width: u16, height: u16, screen: Rect) -> Rect {
    let width = width.min(screen.width.saturating_sub(2));
    let x = at.0.min(screen.right().saturating_sub(width + 1));
    // Leave the two footer rows visible.
    let below = screen.bottom().saturating_sub(at.1 + 3);
    let above = at.1.saturating_sub(screen.y + 1);
    if below >= height || below >= above {
        Rect::new(x, at.1 + 1, width, height.min(below))
    } else {
        let height = height.min(above);
        Rect::new(x, at.1 - height, width, height)
    }
}

fn completion_menu(frame: &mut Frame, app: &App, at: (u16, u16), screen: Rect) {
    let Some(menu) = &app.menu else {
        return;
    };
    let theme = &app.theme;
    let rows = menu.shown.len().min(10);
    let top = menu.selected.saturating_sub(rows - 1);
    let visible = &menu.shown[top..top + rows];
    let label_width = visible
        .iter()
        .map(|&i| menu.items[i].label.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(8, 40);
    let detail_width = visible
        .iter()
        .map(|&i| menu.items[i].detail.chars().count())
        .max()
        .unwrap_or(0)
        .min(36);
    let width = (label_width + detail_width + 5) as u16;
    // Align the list with the start of the word being completed.
    let b = &app.buffers[menu.buffer];
    let shift = b.col.saturating_sub(menu.start) as u16;
    let rect = popup_rect(
        (at.0.saturating_sub(shift + 1), at.1),
        width,
        rows as u16 + 2,
        screen,
    );
    frame.render_widget(Clear, rect);
    let block = panel(
        format!(" {}/{} ", menu.selected + 1, menu.shown.len()),
        theme,
        true,
    )
    .style(Style::default().bg(color(&theme.background)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let chosen = crate::markdown::mix(color(&theme.selection), color(&theme.accent), 0.45);
    for (offset, &index) in visible.iter().enumerate() {
        let item = &menu.items[index];
        let selected = top + offset == menu.selected;
        let mut label: String = item.label.chars().take(label_width).collect();
        label = format!(" {label:<label_width$} ");
        let detail: String = item.detail.chars().take(detail_width).collect();
        let base = if selected {
            Style::default().bg(chosen)
        } else {
            Style::default()
        };
        let line = Line::from(vec![
            Span::styled(label, base.fg(color(&theme.foreground))),
            Span::styled(
                format!("{detail:<detail_width$} "),
                base.fg(color(&theme.muted)),
            ),
        ]);
        if offset < inner.height as usize {
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(inner.x, inner.y + offset as u16, inner.width, 1),
            );
        }
    }
}

fn hover_popup(frame: &mut Frame, app: &App, hover: &str, at: (u16, u16), screen: Rect) {
    let theme = &app.theme;
    let width = 78.min(screen.width.saturating_sub(4)) as usize;
    let doc = crate::markdown::render(hover, width.saturating_sub(4), theme, &app.syntax);
    let used = doc
        .lines
        .iter()
        .map(|l| l.width())
        .max()
        .unwrap_or(10)
        .max(16);
    let height = doc.lines.len().clamp(1, 18);
    let rect = popup_rect((at.0, at.1), (used + 4) as u16, height as u16 + 2, screen);
    frame.render_widget(Clear, rect);
    let more = doc.lines.len() + 2 > rect.height as usize;
    let block = panel(if more { " more… " } else { "" }, theme, true)
        .style(Style::default().bg(color(&theme.background)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    for (offset, line) in doc.lines.iter().take(inner.height as usize).enumerate() {
        frame.render_widget(
            Paragraph::new(line.clone()),
            Rect::new(
                inner.x + 1,
                inner.y + offset as u16,
                inner.width.saturating_sub(2),
                1,
            ),
        );
    }
}

fn display_width(chars: impl Iterator<Item = char>, tab: usize) -> usize {
    chars.fold(0, |column, c| {
        column
            + if c == '\t' {
                tab - column % tab
            } else {
                UnicodeWidthChar::width(c).unwrap_or(0)
            }
    })
}

fn clip(value: &str, column: &mut usize, start: usize, width: usize, tab: usize) -> String {
    let mut out = String::new();
    for c in value.chars() {
        if c == '\n' || c == '\r' {
            continue;
        }
        let size = if c == '\t' {
            tab - *column % tab
        } else {
            UnicodeWidthChar::width(c).unwrap_or(0)
        };
        let end = *column + size;
        if c == '\t' {
            let visible = end.min(start + width).saturating_sub((*column).max(start));
            out.push_str(&" ".repeat(visible));
        } else if *column >= start && end <= start + width && !c.is_control() {
            out.push(c);
        } else if end > start && *column < start + width {
            out.push_str(&" ".repeat(end.min(start + width).saturating_sub((*column).max(start))));
        }
        *column = end;
    }
    out
}

fn term_color(value: vt100::Color, fallback: Color, theme: &Theme) -> Color {
    match value {
        vt100::Color::Default => fallback,
        vt100::Color::Idx(n @ 0..=15) if theme.ansi.is_some() => {
            color(&theme.ansi.as_ref().expect("palette checked")[n as usize])
        }
        vt100::Color::Idx(n) => Color::Indexed(n),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn ai(frame: &mut Frame, area: Rect, theme: &Theme, customs: &[String]) {
    let fg = Style::default().fg(color(&theme.foreground));
    let muted = Style::default().fg(color(&theme.muted));
    let accent = Style::default()
        .fg(color(&theme.accent))
        .add_modifier(Modifier::BOLD);
    let mut lines = vec![
        Line::from(""),
        Line::styled("  YOUR TOOLS. YOUR KEYS.", accent),
        Line::from(""),
        Line::styled("  [1]  OpenAI Codex", fg),
        Line::styled("  [2]  Claude Code", fg),
        Line::styled("  [3]  Gemini CLI", fg),
        Line::styled("  [4]  Antigravity CLI (agy)", fg),
        Line::styled("  [5]  Local LLM (yours, or Ollama/LM Studio)", fg),
    ];
    // Your own [ai.<name>] launchers take keys 6–9.
    for (i, name) in customs.iter().take(4).enumerate() {
        lines.push(Line::styled(format!("  [{}]  {name}", i + 6), fg));
    }
    lines.extend([
        Line::from(""),
        Line::styled("  Native CLI · real terminal", muted),
        Line::styled("  Same project. Live file refresh.", muted),
        Line::from(""),
        Line::styled("  Sign in using the CLI's own flow.", muted),
        Line::styled("  Odin never stores API keys.", muted),
        Line::from(""),
        Line::styled(
            "  Focus: Ctrl-b → / o",
            Style::default().fg(color(&theme.secondary)),
        ),
        Line::styled(
            "  Launch: press a number · add your own: [ai.name]",
            Style::default().fg(color(&theme.secondary)),
        ),
    ]);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// Lane colors for the commit graph: the theme's own accents, then its ANSI palette.
fn lane_colors(theme: &Theme) -> Vec<Color> {
    let mut colors = vec![
        color(&theme.accent),
        color(&theme.secondary),
        color(&theme.warning),
    ];
    match &theme.ansi {
        Some(ansi) => colors.extend([13, 14, 9, 10, 12, 11].map(|i| color(&ansi[i]))),
        None => colors.extend([
            Color::Rgb(0xc6, 0x78, 0xdd),
            Color::Rgb(0x56, 0xb6, 0xc2),
            Color::Rgb(0xe0, 0x6c, 0x75),
            Color::Rgb(0x98, 0xc3, 0x79),
        ]),
    }
    colors
}

/// Keep `selected` inside a `height`-row window that starts at `scroll`.
fn follow(scroll: &mut usize, selected: usize, height: usize) {
    if selected < *scroll {
        *scroll = selected;
    } else if height > 0 && selected >= *scroll + height {
        *scroll = selected + 1 - height;
    }
}

/// `ahead 2, behind 1` → `↑2 ↓1`.
fn track(track: &str) -> String {
    track
        .split(", ")
        .filter(|t| !t.is_empty())
        .map(|t| {
            if let Some(n) = t.strip_prefix("ahead ") {
                format!("↑{n}")
            } else if let Some(n) = t.strip_prefix("behind ") {
                format!("↓{n}")
            } else {
                t.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Fit `spans` into `width` columns, then right-align `right` when it still fits.
fn fit_line<'a>(spans: Vec<Span<'a>>, right: Span<'a>, width: usize, fill: Style) -> Line<'a> {
    let mut out = Vec::new();
    let mut used = 0;
    let right_width = display_width(right.content.chars(), 4);
    for span in spans {
        let w = display_width(span.content.chars(), 4);
        if used + w <= width {
            used += w;
            out.push(span);
            continue;
        }
        let mut column = 0;
        let room = width.saturating_sub(used + 1);
        let cut = clip(&span.content, &mut column, 0, room, 4);
        used += display_width(cut.chars(), 4) + 1;
        out.push(Span::styled(format!("{cut}…"), span.style));
        break;
    }
    if used + right_width + 2 <= width {
        out.push(Span::styled(" ".repeat(width - used - right_width), fill));
        out.push(right);
    } else if used < width {
        out.push(Span::styled(" ".repeat(width - used), fill));
    }
    Line::from(out)
}

fn git_view(frame: &mut Frame, view: &mut GitView, area: Rect, theme: &Theme) {
    let fg = color(&theme.foreground);
    let muted = color(&theme.muted);
    let accent = color(&theme.accent);
    let secondary = color(&theme.secondary);
    let warning = color(&theme.warning);
    let background = color(&theme.background);
    let selection = color(&theme.selection);
    if let Some(error) = &view.data.error {
        text(frame, area, format!(" {error}"), muted);
        return;
    }
    if view.data.commits.is_empty() && view.data.branches.is_empty() {
        text(frame, area, " Reading history…", muted);
        return;
    }
    let width = area.width as usize;

    // Header: where you are and how it compares with upstream.
    let mut header = vec![Span::styled(
        " ⎇ ",
        Style::default().fg(accent).add_modifier(Modifier::BOLD),
    )];
    match view.current_branch() {
        Some(branch) => {
            header.push(Span::styled(
                branch.name.clone(),
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ));
            if !branch.upstream.is_empty() {
                header.push(Span::styled(
                    format!(" → {}", branch.upstream),
                    Style::default().fg(secondary),
                ));
                let state = track(&branch.track);
                header.push(Span::styled(
                    format!(
                        "  {}",
                        if state.is_empty() {
                            "in sync".into()
                        } else {
                            state
                        }
                    ),
                    Style::default().fg(warning),
                ));
            }
        }
        None => header.push(Span::styled(
            format!(
                "detached at {}",
                &view.data.head[..view.data.head.len().min(8)]
            ),
            Style::default().fg(warning),
        )),
    }
    let locals = view.data.branches.iter().filter(|b| !b.remote).count();
    header.push(Span::styled(
        format!(
            "   {locals} local · {} remote branches · {} commits",
            view.data.branches.len() - locals,
            view.data.commits.len()
        ),
        Style::default().fg(muted),
    ));
    frame.render_widget(
        Paragraph::new(fit_line(header, Span::raw(""), width, Style::default())),
        Rect::new(area.x, area.y, area.width, 1),
    );

    let body = Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height.saturating_sub(1),
    );
    let detail_height = if view.details && body.height >= 12 {
        body.height * 2 / 5
    } else {
        0
    };
    let lists = Rect::new(body.x, body.y, body.width, body.height - detail_height);
    let (branch_rect, commit_rect) = if body.width >= 100 {
        let w = 34.min(body.width / 3);
        (
            Rect::new(lists.x, lists.y, w, lists.height),
            Rect::new(lists.x + w + 1, lists.y, lists.width - w - 1, lists.height),
        )
    } else {
        let h = (view.data.branches.len() as u16 + 1)
            .min(lists.height / 3)
            .max(2.min(lists.height));
        (
            Rect::new(lists.x, lists.y, lists.width, h),
            Rect::new(lists.x, lists.y + h, lists.width, lists.height - h),
        )
    };

    // Branches: yours first, then local and remote by recency.
    if branch_rect.height > 0 {
        let focused = view.focus == GitFocus::Branches;
        text(
            frame,
            Rect::new(branch_rect.x, branch_rect.y, branch_rect.width, 1),
            if focused {
                " BRANCHES · Enter shows one"
            } else {
                " BRANCHES · Tab"
            },
            if focused { accent } else { muted },
        );
        let rows = branch_rect.height.saturating_sub(1) as usize;
        follow(&mut view.branch_scroll, view.branch, rows);
        for (offset, (index, branch)) in view
            .data
            .branches
            .iter()
            .enumerate()
            .skip(view.branch_scroll)
            .take(rows)
            .enumerate()
        {
            let selected = index == view.branch;
            let shown = view.only.as_deref() == Some(branch.name.as_str());
            let mut style = Style::default().fg(if branch.remote { secondary } else { fg });
            if branch.current {
                style = style.fg(accent).add_modifier(Modifier::BOLD);
            }
            let fill = if selected && focused {
                Style::default().bg(selection)
            } else {
                Style::default()
            };
            let marker = if branch.current {
                "● "
            } else if shown {
                "▸ "
            } else {
                "  "
            };
            let mut spans = vec![
                Span::styled(format!(" {marker}"), style.patch(fill)),
                Span::styled(branch.name.clone(), style.patch(fill)),
            ];
            let state = track(&branch.track);
            if !state.is_empty() {
                spans.push(Span::styled(
                    format!(" {state}"),
                    Style::default().fg(warning).patch(fill),
                ));
            }
            let right = Span::styled(
                format!("{} ", gitgraph::age(branch.time)),
                Style::default().fg(muted).patch(fill),
            );
            frame.render_widget(
                Paragraph::new(fit_line(spans, right, branch_rect.width as usize, fill)),
                Rect::new(
                    branch_rect.x,
                    branch_rect.y + 1 + offset as u16,
                    branch_rect.width,
                    1,
                ),
            );
        }
    }

    // Commit graph: one color per lane, refs as badges, author and age on the right.
    let rows = commit_rect.height as usize;
    view.height = rows;
    follow(&mut view.scroll, view.commit, rows);
    let lanes = lane_colors(theme);
    let graph_width = view
        .data
        .graph
        .iter()
        .skip(view.scroll)
        .take(rows)
        .map(Vec::len)
        .max()
        .unwrap_or(0)
        .min((commit_rect.width as usize / 3).max(8));
    for (offset, (index, commit)) in view
        .data
        .commits
        .iter()
        .enumerate()
        .skip(view.scroll)
        .take(rows)
        .enumerate()
    {
        let selected = index == view.commit;
        let fill = if selected {
            Style::default().bg(selection)
        } else {
            Style::default()
        };
        let head = commit.hash == view.data.head;
        let mut spans = vec![Span::styled(" ", fill)];
        let cells = view.data.graph.get(index).map(Vec::as_slice).unwrap_or(&[]);
        for (column, (glyph, lane)) in cells.iter().enumerate().take(graph_width) {
            let glyph = if *glyph == '●' && head {
                '◉'
            } else {
                *glyph
            };
            let glyph = if column + 1 == graph_width && cells.len() > graph_width {
                '…'
            } else {
                glyph
            };
            let mut style = Style::default().fg(lanes[lane % lanes.len()]).patch(fill);
            if glyph == '◉' || glyph == '●' {
                style = style.add_modifier(Modifier::BOLD);
            }
            spans.push(Span::styled(glyph.to_string(), style));
        }
        spans.push(Span::styled(
            " ".repeat(graph_width.saturating_sub(cells.len()) + 1),
            fill,
        ));
        spans.push(Span::styled(
            format!("{} ", commit.short),
            Style::default().fg(muted).patch(fill),
        ));
        for r in &commit.refs {
            let (label, style) = match r.kind {
                RefKind::Head => (
                    format!(" ◉ {} ", r.name),
                    Style::default()
                        .fg(background)
                        .bg(accent)
                        .add_modifier(Modifier::BOLD),
                ),
                RefKind::Local => (
                    format!(" {} ", r.name),
                    Style::default()
                        .fg(background)
                        .bg(secondary)
                        .add_modifier(Modifier::BOLD),
                ),
                RefKind::Remote => (r.name.clone(), Style::default().fg(secondary).patch(fill)),
                RefKind::Tag => (
                    format!("◆ {}", r.name),
                    Style::default()
                        .fg(warning)
                        .add_modifier(Modifier::BOLD)
                        .patch(fill),
                ),
            };
            spans.push(Span::styled(label, style));
            spans.push(Span::styled(" ", fill));
        }
        let mut subject = Style::default().fg(fg).patch(fill);
        if head {
            subject = subject.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(commit.subject.clone(), subject));
        let right = Span::styled(
            format!("{} · {} ", commit.author, gitgraph::age(commit.time)),
            Style::default().fg(muted).patch(fill),
        );
        frame.render_widget(
            Paragraph::new(fit_line(spans, right, commit_rect.width as usize, fill)),
            Rect::new(
                commit_rect.x,
                commit_rect.y + offset as u16,
                commit_rect.width,
                1,
            ),
        );
    }

    // Details of the selected commit: full message, author, date and changed files.
    if detail_height > 1 {
        let rect = Rect::new(
            body.x,
            body.bottom() - detail_height,
            body.width,
            detail_height,
        );
        let title = match view.selected() {
            Some(c) => format!("── {} {}", c.short, "─".repeat(width)),
            None => "─".repeat(width),
        };
        text(
            frame,
            Rect::new(rect.x, rect.y, rect.width, 1),
            title,
            color(&theme.border),
        );
        let lines: Vec<Line> = match (&view.detail, view.selected()) {
            (Some((hash, lines)), Some(c)) if *hash == c.hash => lines
                .iter()
                .map(|line| {
                    if let Some((who, when)) = line.split_once('\u{1f}') {
                        Line::from(vec![
                            Span::styled(format!(" {who}"), Style::default().fg(secondary)),
                            Span::styled(format!("  {when}"), Style::default().fg(muted)),
                        ])
                    } else if let Some((file, bar)) = line.split_once(" | ") {
                        let mut spans = vec![Span::styled(
                            format!(" {file} | "),
                            Style::default().fg(muted),
                        )];
                        for ch in bar.chars() {
                            let tint = match ch {
                                '+' => accent,
                                '-' => warning,
                                _ => muted,
                            };
                            spans.push(Span::styled(ch.to_string(), Style::default().fg(tint)));
                        }
                        Line::from(spans)
                    } else if line.contains(" changed") && line.starts_with(' ') {
                        Line::styled(format!(" {line}"), Style::default().fg(muted))
                    } else {
                        Line::styled(format!(" {line}"), Style::default().fg(fg))
                    }
                })
                .collect(),
            (_, Some(_)) => vec![Line::styled(" Loading…", Style::default().fg(muted))],
            _ => Vec::new(),
        };
        frame.render_widget(
            Paragraph::new(lines),
            Rect::new(rect.x, rect.y + 1, rect.width, rect.height - 1),
        );
    }
}

fn footer(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let mode = if app.prefix {
        "PREFIX"
    } else if app.help && app.help_focus {
        "GUIDE"
    } else if app.sidebar_focus {
        if app.git_focus {
            "GIT"
        } else {
            "FILES"
        }
    } else {
        match app.panes.get(&app.focused) {
            Some(Pane::Terminal(_)) => "TERMINAL",
            Some(Pane::Preview(_)) => "PREVIEW",
            Some(Pane::Ai) => "AI",
            Some(Pane::Git(_)) => "GIT",
            _ => match app.mode {
                Mode::Insert => "INSERT",
                Mode::Visual => "VISUAL",
                Mode::VisualLine => "V-LINE",
                Mode::Normal => "NORMAL",
            },
        }
    };
    let mode_color = color(match app.mode {
        Mode::Insert => &theme.warning,
        Mode::Visual | Mode::VisualLine => &theme.secondary,
        Mode::Normal => &theme.accent,
    });
    let lines = Line::from(vec![
        Span::styled(
            format!(" {mode} "),
            Style::default()
                .fg(color(&theme.background))
                .bg(mode_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", printable(&app.message)),
            Style::default().fg(color(if app.error {
                &theme.warning
            } else {
                &theme.foreground
            })),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if let Some(input) = &app.input {
        let prefix = match input.kind {
            InputKind::Command => ":",
            InputKind::Search => "/",
            InputKind::Open => "open > ",
        };
        let text = format!("{prefix}{}", input.text);
        let cols = display_width(text.chars(), 4);
        let skip = cols.saturating_sub(area.width.saturating_sub(1) as usize);
        let mut column = 0;
        let value = clip(&text, &mut column, skip, area.width as usize, 4);
        frame.render_widget(
            Paragraph::new(value).style(Style::default().fg(color(&theme.accent))),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
        frame.set_cursor_position((area.x + (cols - skip) as u16, area.y + 1));
    } else {
        let hints = if app.prefix {
            " % split  \" stack  c shell  a/A AI beside/below  m preview  t themes  arrows focus  z zoom  x close  ? help"
        } else if matches!(app.panes.get(&app.focused), Some(Pane::Git(_))) && !app.sidebar_focus {
            " Tab branches/commits   j k move   Enter show branch / details   a all   y copy hash   r refresh   ^B G maximize"
        } else if matches!(app.panes.get(&app.focused), Some(Pane::Preview(_)))
            && !app.sidebar_focus
        {
            " j k scroll   space ^D ^U page   g G top/end   [ ] headings   e edit   ^B m source"
        } else {
            match app.mode {
                Mode::Visual | Mode::VisualLine => {
                    " move to extend   y/^C copy   d/^X cut   p/^V replace   c change   Esc cancel"
                }
                _ => " ^S save  ^C ^X ^V copy/cut/paste  v select  ^E files  ^P open  ^B panes  F1 help  ^Q quit",
            }
        };
        text(
            frame,
            Rect::new(area.x, area.y + 1, area.width, 1),
            hints,
            color(&theme.muted),
        );
    }
}

/// Greedy word wrap for plain text.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines = vec![String::new()];
    for word in text.split_whitespace() {
        let line = lines.last_mut().expect("one line");
        let used = line.chars().count();
        if used > 0 && used + 1 + word.chars().count() > width {
            lines.push(word.to_string());
        } else {
            if used > 0 {
                line.push(' ');
            }
            line.push_str(word);
        }
    }
    lines
}

fn help(frame: &mut Frame, app: &mut App, rect: Rect) {
    let theme = &app.theme.clone();
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Block::default().style(Style::default().bg(color(&theme.background))),
        rect,
    );
    let title = if rect.width >= 52 {
        if app.help_focus {
            " FIELD GUIDE · ↑↓ scroll · Esc back · F1 closes "
        } else {
            " FIELD GUIDE · F1 to scroll · F1 F1 closes "
        }
    } else {
        " GUIDE · F1 closes "
    };
    let block =
        panel(title, theme, app.help_focus).title_style(Style::default().fg(color(&theme.accent)));
    let inner = block.inner(rect);
    let rows = [
        ("THEMES", "Pick visually — no names to remember"),
        (
            ":themes  or  Ctrl-b t",
            "Open the theme list; ↑↓ previews each theme live",
        ),
        (
            "  type / Enter / Esc",
            "Filter by name / keep and save as default / restore",
        ),
        (":theme name", "Switch directly by name (Tab completes)"),
        (
            "omarchy / omarchy-<name>",
            "Follow the desktop's Omarchy theme / use one of them",
        ),
        ("WORKSPACE", "One prefix. Everything in view."),
        ("Ctrl-b % / \"", "Split side-by-side / top-and-bottom"),
        ("Ctrl-b arrows / h j k l", "Focus adjacent pane; o cycles"),
        ("Ctrl-b Ctrl-arrows", "Resize a split"),
        ("Ctrl-b c", "New shell"),
        (
            "Ctrl-b a / A",
            "AI launcher beside / below (ai_split sets default)",
        ),
        ("Ctrl-b z / x / s", "Zoom / close pane / toggle sidebar"),
        (
            "Ctrl-b g / PgUp / PgDn",
            "Git sidebar / terminal scrollback",
        ),
        (
            ":git  or  Ctrl-b G",
            "Git pane: branches, graph, details; again maximizes",
        ),
        (
            ":session [save|forget]",
            "Reopen this project as you left it (.odin/)",
        ),
        ("Ctrl-b b", "Send literal Ctrl-b to a terminal"),
        (
            "FILES + GIT",
            "Ctrl-e focuses Explorer; Tab switches Files / Git",
        ),
        (
            "↑ ↓ / → / ← / Enter",
            "Browse, previewing files / expand / collapse / open",
        ),
        (". / r / n", "Hidden files / refresh / new file"),
        ("Ctrl-p / Tab", "Open path / complete visible paths"),
        ("EDITOR", "Vim-inspired, not a full Vim implementation"),
        (
            "i a I A o O / Esc",
            "Insert, append, open line / normal mode",
        ),
        ("h j k l / w b / 0 $", "Move / words / line start-end"),
        ("gg / G / 42G / :42", "Top / bottom / jump to line 42"),
        (
            "15j 15k / 15⏎ 15- / :+15",
            "Relative jump: the numbers shown with :set rnu",
        ),
        (
            "x dd D C / yy p P",
            "Delete / change / copy line / paste below, above",
        ),
        (
            "v / V / Shift-arrows",
            "Select characters / lines; y copy, d cut, p replace",
        ),
        (
            "Ctrl-c / Ctrl-x / Ctrl-v",
            "Copy / cut / paste (line if nothing selected)",
        ),
        (
            "LANGUAGE SERVERS",
            "Rust, Go, Python, C, C++ — :lsp shows status",
        ),
        (
            "K / gd / Ctrl-o",
            "Hover docs / go to definition / jump back",
        ),
        (
            "Ctrl-Space / ]d [d / :format",
            "Complete (also after .) / next-prev problem / format",
        ),
        ("u / Ctrl-r / Ctrl-s", "Undo / redo / atomic save"),
        ("/query / n N", "Find / next / previous; wraps around"),
        ("Tab / Shift-Tab", "Cycle open buffers"),
        ("MARKDOWN", ".md files open rendered; edits re-render live"),
        (
            ":preview / Ctrl-b m",
            "Toggle rendered view / source, keeping place",
        ),
        (
            ":preview split / Ctrl-b M",
            "Live preview beside the source",
        ),
        (
            "j k / space ^D ^U / [ ]",
            "Scroll / page / previous-next heading",
        ),
        ("COMMANDS", "Ctrl-b : also works inside terminals"),
        (":w [path] / :wa / :wq", "Save / save all / save and close"),
        (":e path / :reload", "Open or create / reload a clean file"),
        (":q / :qa / :qa!", "Close / quit / discard and quit"),
        (
            ":ai codex|claude|gemini|agy",
            "Run an AI CLI here; add `below` or `right` to place it",
        ),
        (
            ":ai local [model]",
            "Local LLM: [ai.local] in config, else Ollama / LM Studio",
        ),
        (
            ":ai <name> [model]",
            "Any tool you add as [ai.<name>] (keys 6–9 in launcher)",
        ),
        (":run name", "Launch a configured command plugin"),
        (":set rnu / :set nornu", "Relative / absolute line numbers"),
    ];
    // Keys on the left, descriptions wrapped beside them; a key too long for its
    // column gets its own line.
    let width = inner.width as usize;
    let key_col = if width >= 72 {
        29
    } else {
        (width / 2).clamp(12, 24)
    };
    let key_style = Style::default().fg(color(&theme.accent));
    let text_style = Style::default().fg(color(&theme.foreground));
    let mut lines = Vec::new();
    for (key, description) in rows {
        let heading = key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == ' ' || c == '+');
        if heading {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.push(Line::from(Span::styled(
                format!(" {key}"),
                Style::default()
                    .fg(color(&theme.secondary))
                    .add_modifier(Modifier::BOLD),
            )));
            for part in wrap_words(description, width.saturating_sub(2)) {
                lines.push(Line::from(Span::styled(
                    format!(" {part}"),
                    Style::default().fg(color(&theme.muted)),
                )));
            }
            continue;
        }
        let beside = width.saturating_sub(key_col + 2);
        if key.chars().count() < key_col && beside >= 14 {
            for (i, part) in wrap_words(description, beside).into_iter().enumerate() {
                let label = if i == 0 { key } else { "" };
                lines.push(Line::from(vec![
                    Span::styled(format!(" {label:<key_col$}"), key_style),
                    Span::styled(part, text_style),
                ]));
            }
        } else {
            lines.push(Line::from(Span::styled(format!(" {key}"), key_style)));
            for part in wrap_words(description, width.saturating_sub(5)) {
                lines.push(Line::from(Span::styled(format!("    {part}"), text_style)));
            }
        }
    }
    let total = lines.len();
    let max = total.saturating_sub(inner.height as usize);
    app.help_scroll = app.help_scroll.min(max);
    // Say when there is more, and how to reach it.
    let block = if max > 0 {
        let arrows = match (app.help_scroll > 0, app.help_scroll < max) {
            (true, true) => "▲▼",
            (false, _) => "▼",
            (true, false) => "▲",
        };
        block.title_bottom(
            Line::styled(
                format!(
                    " {arrows} more · {} · {}% ",
                    if app.help_focus {
                        "↑↓ PgDn"
                    } else {
                        "F1 then ↑↓"
                    },
                    app.help_scroll * 100 / max
                ),
                Style::default().fg(color(&theme.accent)),
            )
            .right_aligned(),
        )
    } else {
        block
    };
    frame.render_widget(block, rect);
    let visible: Vec<Line> = lines.into_iter().skip(app.help_scroll).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_palette_maps_ansi_but_preserves_extended_and_truecolor() {
        let theme = crate::config::load_theme_from("visual-studio-dark", None).unwrap();
        let fallback = color(&theme.foreground);
        assert_eq!(
            term_color(vt100::Color::Default, fallback, &theme),
            fallback
        );
        for index in 0..16 {
            assert_eq!(
                term_color(vt100::Color::Idx(index), fallback, &theme),
                color(&theme.ansi.as_ref().unwrap()[index as usize])
            );
        }
        assert_eq!(
            term_color(vt100::Color::Idx(200), fallback, &theme),
            Color::Indexed(200)
        );
        assert_eq!(
            term_color(vt100::Color::Rgb(1, 2, 3), fallback, &theme),
            Color::Rgb(1, 2, 3)
        );
        assert_eq!(
            term_color(vt100::Color::Idx(1), fallback, &Theme::default()),
            Color::Indexed(1)
        );
    }
    #[test]
    fn clipping_handles_tabs_and_wide_chars_without_splitting_cells() {
        let mut column = 0;
        assert_eq!(clip("a\t界z", &mut column, 2, 4, 4), "  界");
        assert_eq!(display_width("a\t界z".chars(), 4), 7);
    }
}
