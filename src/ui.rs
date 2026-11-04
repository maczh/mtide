use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Tabs, Wrap},
    Frame,
};
use std::sync::{OnceLock, RwLock};

use crate::app::{App, BottomTab, Focus, LspPopup, PluginDialog, SidebarTab};
use crate::config::Theme;
use crate::git::ConflictBlock;
use crate::lsp::LspServerState;
use crate::terminal::{AnsiColor, CursorShape, TerminalStyle};
use unicode_width::UnicodeWidthChar;

static ACTIVE_THEME: OnceLock<RwLock<Theme>> = OnceLock::new();

fn active_theme() -> Theme {
    ACTIVE_THEME
        .get_or_init(|| RwLock::new(Theme::default()))
        .read()
        .map_or_else(|_| Theme::default(), |theme| *theme)
}

fn conflict_line_marker(blocks: &[ConflictBlock], line: usize) -> &'static str {
    for block in blocks {
        if line == block.start_line
            || line == block.end_line
            || line == block.ours_end
            || line + 1 == block.theirs_start
        {
            return "!";
        }
        if block.base_start == Some(line) {
            return "B";
        }
        if (block.ours_start..block.ours_end).contains(&line) {
            return "O";
        }
        if let (Some(base_start), Some(base_end)) = (block.base_start, block.base_end) {
            if (base_start..base_end).contains(&line) {
                return "B";
            }
        }
        if (block.theirs_start..block.theirs_end).contains(&line) {
            return "T";
        }
    }
    " "
}

fn set_active_theme(theme: Theme) {
    let lock = ACTIVE_THEME.get_or_init(|| RwLock::new(Theme::default()));
    if let Ok(mut active) = lock.write() {
        *active = theme;
    }
}

pub fn draw(frame: &mut Frame<'_>, app: &App) {
    set_active_theme(app.theme());
    let theme = active_theme();
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().fg(theme.foreground).bg(theme.background)),
        area,
    );

    if area.width < 50 || area.height < 10 {
        draw_small(frame, app, area);
        return;
    }

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area);
    draw_menu(frame, vertical[0], app);
    draw_main(frame, vertical[1], app);
    draw_status(frame, vertical[2], app);
    draw_plugin_drawer(frame, area, app);
    if app.plugin_palette_open() {
        draw_plugin_palette(frame, area, app);
    }
    if app.settings_overlay_open() {
        draw_settings_overlay(frame, area, app);
    }
    if app.quick_open_open() {
        draw_quick_open(frame, area, app);
    }
    if app.explorer_mode().is_some() {
        draw_explorer_input(frame, area, app);
    }
    if app.goto_mode() {
        draw_goto(frame, area, app);
    }
    if app.plugin_dialog().is_some() {
        draw_plugin_dialog(frame, area, app);
    }
    if app.plugin_command_line().is_some() {
        draw_plugin_command_line(frame, area, app);
    }
    if app.git_credential_prompt().is_some() {
        draw_git_credential_prompt(frame, area, app);
    }
}

fn draw_git_credential_prompt(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = active_theme();
    let Some(prompt) = app.git_credential_prompt() else {
        return;
    };
    let width = 60.min(area.width.saturating_sub(2)).max(30);
    let height = 8.min(area.height).max(5);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 3,
        width,
        height,
    );
    let username_marker = if prompt.password_focus { " " } else { "▶" };
    let password_marker = if prompt.password_focus { "▶" } else { " " };
    let password = "•".repeat(prompt.password.chars().count());
    let text = format!(
        "Operation: {}\n{} Username: {}\n{} Password: {}\n\nTab switch · Enter retry · Esc cancel",
        prompt.operation, username_marker, prompt.username, password_marker, password
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(theme.foreground).bg(theme.panel))
            .block(panel("Git credentials · memory only", true))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_plugin_command_line(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = active_theme();
    let Some(command_line) = app.plugin_command_line() else {
        return;
    };
    let height = 3;
    if area.height < height {
        return;
    }
    let prompt = format!("{}{}", command_line.prefix, command_line.text);
    let popup = Rect::new(area.x, area.y + area.height - height, area.width, height);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(prompt)
            .style(Style::default().fg(theme.foreground).bg(theme.panel))
            .block(panel("Command line", true)),
        popup,
    );
}

fn draw_plugin_dialog(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = active_theme();
    let Some(popup) = plugin_dialog_rect(area, app) else {
        return;
    };
    let (title, text) = match app.plugin_dialog() {
        Some(PluginDialog::Confirm { message, .. }) => (
            "Confirm",
            format!("{message}\n\nEnter confirm · Esc cancel"),
        ),
        Some(PluginDialog::Info { title, entries }) => {
            let body = entries
                .iter()
                .map(|(key, value)| format!("{key}: {value}"))
                .collect::<Vec<_>>()
                .join("\n");
            (title.as_str(), format!("{body}\n\nEnter/Esc close"))
        }
        Some(PluginDialog::Custom { title, .. }) => (
            title.as_str(),
            app.plugin_dialog_content().unwrap_or_default(),
        ),
        None => return,
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(theme.foreground).bg(theme.panel))
            .wrap(Wrap { trim: false })
            .block(panel(title, true)),
        popup,
    );
}

fn plugin_dialog_rect(area: Rect, app: &App) -> Option<Rect> {
    app.plugin_dialog()?;
    let (requested_width, requested_height) = match app.plugin_dialog() {
        Some(PluginDialog::Custom { width, height, .. }) => (*width, *height + 2),
        _ => (72, 16),
    };
    let width = requested_width.min(area.width.saturating_sub(2)).max(8);
    let height = requested_height.min(area.height).max(3);
    Some(Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 3,
        width,
        height,
    ))
}

fn draw_plugin_drawer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = active_theme();
    let Some((side, requested_width, min_width, text)) = app.plugin_drawer() else {
        return;
    };
    let max_width = area.width.saturating_sub(4);
    if max_width == 0 || area.height < 4 {
        return;
    }
    let width = requested_width.max(min_width).min(max_width).max(1);
    let drawer = Rect::new(
        if side == "left" {
            1
        } else {
            area.right().saturating_sub(width + 1)
        },
        1,
        width,
        area.height.saturating_sub(2),
    );
    frame.render_widget(Clear, drawer);
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(theme.foreground).bg(theme.panel))
            .wrap(Wrap { trim: false })
            .block(panel("Plugin Drawer", true)),
        drawer,
    );
}

pub fn handle_mouse(app: &mut App, event: MouseEvent, area: Rect) {
    if app.plugin_dialog().is_some() {
        let Some(popup) = plugin_dialog_rect(area, app) else {
            return;
        };
        if matches!(event.kind, MouseEventKind::Down(MouseButton::Left))
            && event.column >= popup.x
            && event.column < popup.right()
            && event.row >= popup.y
            && event.row < popup.bottom()
        {
            let content_line = event.row.saturating_sub(popup.y.saturating_add(1)) as usize;
            if let Err(error) = app.plugin_dialog_mouse_click(content_line) {
                app.set_status_message(format!("Plugin dialog click failed: {error}"));
            }
        }
        return;
    }
    if let Some(index) = menu_hit(area, event.column, event.row) {
        if matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) {
            app.menu_click(index);
        }
        return;
    }
    if let Some(sidebar) = sidebar_area(app, area) {
        if event.column >= sidebar.x
            && event.column < sidebar.right()
            && event.row >= sidebar.y
            && event.row < sidebar.bottom()
        {
            let bottom_height = if app.sidebar_tab() == SidebarTab::Explorer {
                Constraint::Length(0)
            } else {
                Constraint::Length(3)
            };
            let parts = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(2), Constraint::Min(3), bottom_height])
                .split(sidebar);
            let body = parts[1];
            if event.row < body.y {
                if !matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) {
                    return;
                }
                let tab_width = usize::from(sidebar.width.max(1)) / 4;
                let index = usize::from(event.column.saturating_sub(sidebar.x)) / tab_width.max(1);
                app.select_sidebar_tab_by_index(index.min(3));
            } else if app.sidebar_tab() == SidebarTab::Explorer
                && event.row >= body.y.saturating_add(1)
            {
                let index = usize::from(event.row.saturating_sub(body.y).saturating_sub(1));
                match event.kind {
                    MouseEventKind::Moved => app.explorer_hover(index),
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Err(error) = app.explorer_click(index) {
                            app.set_status_message(format!("Explorer click failed: {error}"));
                        }
                    }
                    _ => {}
                }
            }
        }
        return;
    }
    if let Some(agent) = agent_area(app, area) {
        if event.column >= agent.x
            && event.column < agent.right()
            && event.row >= agent.y
            && event.row < agent.bottom()
        {
            app.set_focus(Focus::Agent);
        }
    } else if let Some(bottom) = terminal_area(app, area) {
        if event.column >= bottom.x
            && event.column < bottom.right()
            && event.row >= bottom.y
            && event.row < bottom.bottom()
        {
            app.set_focus(if app.bottom_tab() == crate::app::BottomTab::Output {
                Focus::Output
            } else {
                Focus::Terminal
            });
        }
    } else if matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) {
        // Any click in the center (including editor tabs) returns keyboard focus
        // to the editor before the region-specific mouse handler runs.
        app.set_focus(Focus::Editor);
    }
    if app.focus() == Focus::Agent {
        let Some(agent) = agent_area(app, area) else {
            return;
        };
        if event.column < agent.x
            || event.column >= agent.right()
            || event.row < agent.y
            || event.row >= agent.bottom()
        {
            return;
        }
        if app.agent_mouse_tracking() {
            let column = event.column.saturating_sub(agent.x.saturating_add(1));
            let row = event.row.saturating_sub(agent.y.saturating_add(1));
            if let Err(error) = app.send_agent_mouse(column, row, event.kind, event.modifiers) {
                app.set_status_message(format!("Agent mouse error: {error}"));
            }
        }
        return;
    }
    if app.focus() == Focus::Terminal {
        let Some(terminal) = terminal_area(app, area) else {
            return;
        };
        if event.column < terminal.x
            || event.column >= terminal.right()
            || event.row < terminal.y
            || event.row >= terminal.bottom()
        {
            return;
        }
        let column = event.column.saturating_sub(terminal.x.saturating_add(1));
        let row = event.row.saturating_sub(terminal.y.saturating_add(1));
        if matches!(event.kind, MouseEventKind::Down(MouseButton::Left))
            && !app.terminal_mouse_tracking()
        {
            if let Some(url) = app.terminal_link_at(column, row) {
                app.open_terminal_link(&url);
                return;
            }
        }
        if app.terminal_mouse_tracking() {
            if let Err(error) = app.send_terminal_mouse(column, row, event.kind, event.modifiers) {
                app.set_status_message(format!("Terminal mouse error: {error}"));
            }
            return;
        }
        match event.kind {
            MouseEventKind::ScrollUp => app.scroll_terminal(3),
            MouseEventKind::ScrollDown => app.scroll_terminal(-3),
            _ => {}
        }
        return;
    }
    let position = editor_mouse_position(app, area, event.column, event.row);
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(position) = position {
                if app.column_selection_active()
                    || (event
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::ALT)
                        && event
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::SHIFT))
                {
                    app.mouse_column_down(position);
                } else if event
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::ALT)
                {
                    app.mouse_add_cursor(position);
                } else {
                    app.mouse_down(
                        position,
                        event
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::SHIFT),
                    );
                }
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(position) = position {
                if event
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::ALT)
                    && event
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::SHIFT)
                {
                    app.mouse_column_drag(position);
                } else {
                    app.mouse_drag(position);
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => app.mouse_up(),
        _ => {}
    }
}

fn menu_hit(area: Rect, column: u16, row: u16) -> Option<usize> {
    if area.width < 50 || area.height < 10 || row != area.y {
        return None;
    }
    let offset = column.checked_sub(area.x.saturating_add(6))?;
    if offset >= 31 {
        return None;
    }
    Some(match offset {
        0..=6 => 0,
        7..=12 => 1,
        13..=18 => 2,
        19..=27 => 3,
        _ => 4,
    })
}

fn sidebar_area(app: &App, area: Rect) -> Option<Rect> {
    if !app.sidebar_visible() || area.width < 50 || area.height < 10 {
        return None;
    }
    let main = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area)[1];
    Some(
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(app.sidebar_width()), Constraint::Min(28)])
            .split(main)[0],
    )
}

fn terminal_area(app: &App, area: Rect) -> Option<Rect> {
    if area.width < 50 || area.height < 10 || !app.bottom_visible() {
        return None;
    }
    let main = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area)[1];
    let mut horizontal = Vec::new();
    if app.sidebar_visible() {
        horizontal.push(Constraint::Length(app.sidebar_width()));
    }
    horizontal.push(Constraint::Min(28));
    if app.agent_visible() && main.width >= 105 {
        horizontal.push(Constraint::Length(app.agent_width()));
    }
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(horizontal)
        .split(main);
    let center = *columns.get(usize::from(app.sidebar_visible()))?;
    Some(
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(3),
                Constraint::Length(5),
            ])
            .split(center)[2],
    )
}

fn agent_area(app: &App, area: Rect) -> Option<Rect> {
    if area.width < 50 || area.height < 10 || !app.agent_visible() || area.width < 105 {
        return None;
    }
    let main = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area)[1];
    let mut horizontal = Vec::new();
    if app.sidebar_visible() {
        horizontal.push(Constraint::Length(app.sidebar_width()));
    }
    horizontal.push(Constraint::Min(28));
    horizontal.push(Constraint::Length(app.agent_width()));
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints(horizontal)
        .split(main)
        .last()
        .copied()
}

fn editor_mouse_position(
    app: &App,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<crate::core::Position> {
    if area.width < 50 || area.height < 10 {
        return None;
    }
    let main = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area)[1];
    let mut horizontal = Vec::new();
    if app.sidebar_visible() {
        horizontal.push(Constraint::Length(app.sidebar_width()));
    }
    horizontal.push(Constraint::Min(28));
    if app.agent_visible() && main.width >= 105 {
        horizontal.push(Constraint::Length(app.agent_width()));
    }
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(horizontal)
        .split(main);
    let center_index = usize::from(app.sidebar_visible());
    let center = *columns.get(center_index)?;
    let center_constraints = if app.bottom_visible() {
        vec![
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(5),
        ]
    } else {
        vec![Constraint::Length(2), Constraint::Min(3)]
    };
    let editor = Layout::default()
        .direction(Direction::Vertical)
        .constraints(center_constraints)
        .split(center)[1];
    if column < editor.x.saturating_add(7)
        || column >= editor.right()
        || row <= editor.y
        || row >= editor.bottom()
    {
        return None;
    }
    let content_width = editor.width.saturating_sub(8) as usize;
    let scroll_x = app.horizontal_scroll().max(
        app.cursor()
            .column
            .saturating_sub(content_width.saturating_sub(1)),
    );
    let scroll_y = app
        .cursor()
        .line
        .saturating_sub(editor.height.saturating_sub(3) as usize);
    let line = (scroll_y + row.saturating_sub(editor.y + 1) as usize)
        .min(app.file_lines().len().saturating_sub(1));
    let column = (scroll_x + column.saturating_sub(editor.x + 7) as usize)
        .min(app.file_lines()[line].chars().count());
    Some(crate::core::Position { line, column })
}

fn draw_menu(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = active_theme();
    let title = Line::from(vec![
        Span::styled(
            " mtide ",
            Style::default()
                .fg(theme.background)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  File  Edit  View  Options  Help ",
            Style::default().fg(theme.muted),
        ),
        Span::styled(
            format!(
                " · {} · LSP:{} · theme:{}{} ",
                app.focus().label(),
                match app.lsp_state() {
                    Some(LspServerState::Starting) => "starting",
                    Some(LspServerState::Connected) => "ready",
                    Some(LspServerState::Failed) => "failed",
                    None if app.lsp_server_count() == 0 => "off",
                    None => "idle",
                },
                app.theme_name(),
                app.plugin_summary().map_or(
                    String::new(),
                    |(count, commands, keybindings, panels)| {
                        format!(
                            " · plugins:{} c:{} k:{} p:{}",
                            count, commands, keybindings, panels
                        )
                    }
                )
            ),
            Style::default().fg(theme.positive),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(title).style(Style::default().bg(theme.panel)),
        area,
    );
}

fn draw_main(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut horizontal = Vec::new();
    if app.sidebar_visible() {
        horizontal.push(Constraint::Length(app.sidebar_width()));
    }
    horizontal.push(Constraint::Min(28));
    if app.agent_visible() && area.width >= 105 {
        horizontal.push(Constraint::Length(app.agent_width()));
    }
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(horizontal)
        .split(area);
    let mut index = 0;
    if app.sidebar_visible() {
        draw_sidebar(frame, columns[index], app);
        index += 1;
    }
    draw_center(frame, columns[index], app);
    index += 1;
    if index < columns.len() {
        draw_agent(frame, columns[index], app);
    }
}

fn draw_sidebar(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = active_theme();
    let bottom_height = if app.sidebar_tab() == SidebarTab::Explorer {
        Constraint::Length(0)
    } else {
        Constraint::Length(3)
    };
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(3), bottom_height])
        .split(area);
    let tabs = Tabs::new([
        SidebarTab::Explorer.label(),
        SidebarTab::Search.label(),
        SidebarTab::Changes.label(),
        SidebarTab::Branches.label(),
    ])
    .select(match app.sidebar_tab() {
        SidebarTab::Explorer => 0,
        SidebarTab::Search => 1,
        SidebarTab::Changes => 2,
        SidebarTab::Branches => 3,
    })
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border)),
    )
    .style(Style::default().fg(theme.muted))
    .highlight_style(
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(tabs, vertical[0]);

    let body: Vec<&str> = match app.sidebar_tab() {
        SidebarTab::Explorer => Vec::new(),
        SidebarTab::Search => Vec::new(),
        SidebarTab::Changes => Vec::new(),
        SidebarTab::Branches => Vec::new(),
    };
    let lines = body.into_iter().map(|line| {
        Line::from(Span::styled(
            line,
            Style::default().fg(if app.focus() == Focus::Explorer {
                theme.foreground
            } else {
                theme.muted
            }),
        ))
    });
    let mut sidebar_text = Text::from_iter(lines);
    if app.sidebar_tab() == SidebarTab::Search {
        sidebar_text.lines.push(Line::from(Span::styled(
            format!("Search: {}", app.search_query()),
            Style::default().fg(theme.foreground),
        )));
        if app.replace_mode() {
            sidebar_text.lines.push(Line::from(Span::styled(
                format!("Replace: {}", app.replace_query()),
                Style::default().fg(theme.warning),
            )));
            sidebar_text.lines.push(Line::from(Span::styled(
                "Enter: current file · Ctrl-Shift-Enter: workspace",
                Style::default().fg(theme.muted),
            )));
        }
        if app.search_results().is_empty() {
            sidebar_text.lines.push(Line::from(Span::styled(
                "No matches",
                Style::default().fg(theme.muted),
            )));
        } else {
            for (index, result) in app.search_results().iter().take(8).enumerate() {
                let style = if index == app.search_index() {
                    Style::default().fg(theme.background).bg(theme.accent)
                } else {
                    Style::default().fg(theme.accent)
                };
                sidebar_text.lines.push(Line::from(Span::styled(
                    format!(
                        "{}:{}:{}",
                        result.path.display(),
                        result.line,
                        result.column
                    ),
                    style,
                )));
            }
        }
    } else if app.sidebar_tab() == SidebarTab::Explorer {
        sidebar_text.lines.push(Line::from(Span::styled(
            "Workspace",
            Style::default().fg(theme.foreground),
        )));
        for (index, path) in app.explorer_paths().iter().enumerate().take(14) {
            let marker = if path.is_dir() {
                if app.explorer_expanded(path) {
                    "▾"
                } else {
                    "▸"
                }
            } else {
                " "
            };
            let indent = "  ".repeat(app.explorer_depth(path));
            let style = if index == app.explorer_index() {
                Style::default().fg(theme.background).bg(theme.accent)
            } else {
                Style::default().fg(theme.foreground)
            };
            sidebar_text.lines.push(Line::from(Span::styled(
                format!("{indent}{marker} {}", app.explorer_label(path)),
                style,
            )));
        }
    } else if app.sidebar_tab() == SidebarTab::Changes {
        sidebar_text.lines.push(Line::from(Span::styled(
            "Changes",
            Style::default().fg(theme.foreground),
        )));
        if app.changes().is_empty() {
            sidebar_text.lines.push(Line::from(Span::styled(
                "Working tree clean",
                Style::default().fg(theme.positive),
            )));
        } else {
            for (index, change) in app.changes().iter().take(12).enumerate() {
                let style = if index == app.change_index() {
                    Style::default().fg(theme.background).bg(theme.accent)
                } else {
                    Style::default().fg(theme.foreground)
                };
                sidebar_text.lines.push(Line::from(Span::styled(
                    format!("{}{} {}", change.index, change.worktree, change.path),
                    if change.is_conflict() {
                        style.fg(theme.error)
                    } else {
                        style
                    },
                )));
            }
        }
        sidebar_text.lines.push(Line::from(Span::styled(
            if app.commit_mode() {
                format!("Commit: {}", app.commit_message())
            } else {
                format!(
                    "c commit · b blame · l log · d diff · s/u file · h/H hunk · [/ ] hunk {}/{} · o/t ours/theirs · f/p/P sync",
                    app.change_hunk_index() + 1,
                    app.change_hunk_count().max(1)
                )
            },
            Style::default().fg(if app.commit_mode() { theme.accent } else { theme.muted }),
        )));
    } else if app.sidebar_tab() == SidebarTab::Branches {
        sidebar_text.lines.push(Line::from(Span::styled(
            "Branches",
            Style::default().fg(theme.foreground),
        )));
        if app.branch_mode() {
            sidebar_text.lines.push(Line::from(Span::styled(
                format!("New: {}", app.branch_name()),
                Style::default().fg(theme.accent),
            )));
        }
        if app.branches().is_empty() {
            sidebar_text.lines.push(Line::from(Span::styled(
                "No local branches",
                Style::default().fg(theme.muted),
            )));
        } else {
            for (index, branch) in app.branches().iter().enumerate().take(14) {
                let style = if index == app.branch_index() {
                    Style::default().fg(theme.background).bg(theme.accent)
                } else {
                    Style::default().fg(theme.foreground)
                };
                sidebar_text.lines.push(Line::from(Span::styled(
                    format!(
                        "{} {}",
                        if Some(branch.as_str()) == app.branch() {
                            "*"
                        } else {
                            " "
                        },
                        branch
                    ),
                    style,
                )));
            }
        }
        sidebar_text.lines.push(Line::from(Span::styled(
            "n new · d delete · Enter switch",
            Style::default().fg(theme.muted),
        )));
    }
    if let Some(panel_text) = app.plugin_panel_text() {
        sidebar_text.lines.push(Line::from(Span::styled(
            "Plugin panel",
            Style::default().fg(theme.positive),
        )));
        sidebar_text
            .lines
            .extend(panel_text.lines().map(|line| Line::from(line.to_owned())));
    }
    frame.render_widget(
        Paragraph::new(sidebar_text).block(panel("Sidebar", app.focus() != Focus::Editor)),
        vertical[1],
    );

    frame.render_widget(
        Paragraph::new(
            "1 Explore 2 Find\n3 Changes 4 Branches\n5 Terminal 6 Output\nCtrl-Alt-T theme",
        )
        .style(Style::default().fg(theme.muted))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border)),
        ),
        vertical[2],
    );
}

fn draw_center(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = active_theme();
    let constraints = if app.bottom_visible() {
        vec![
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(5),
        ]
    } else {
        vec![Constraint::Length(2), Constraint::Min(3)]
    };
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let tabs = Tabs::new(app.tab_titles())
        .select(app.active_tab())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border)),
        )
        .style(Style::default().fg(theme.muted))
        .highlight_style(
            Style::default()
                .fg(theme.foreground)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, vertical[0]);

    let content_width = vertical[1].width.saturating_sub(8) as usize;
    let cursor = app.cursor();
    let wrap_width = app
        .word_wrap()
        .then(|| app.word_wrap_column().min(content_width.max(1)));
    let cursor_display_column =
        display_column(app.file_lines()[cursor.line].as_str(), cursor.column);
    let horizontal_scroll = app
        .horizontal_scroll()
        .max(cursor_display_column.saturating_sub(content_width.saturating_sub(1)));
    let mut cursor_visual_row = 0usize;
    let mut cursor_visual_column = 0usize;
    let mut visual_row = 0usize;
    let mut editor_lines = Vec::new();
    let diff_markers = app.diff_line_markers();
    let conflict_blocks = crate::git::conflict_blocks(app.file_lines());
    let cursor_positions = app.cursor_positions();
    for (index, line) in app.file_lines().iter().enumerate() {
        if app.is_line_hidden(index) {
            continue;
        }
        let selected = app.is_line_selected(index);
        let diff_marker = diff_markers.get(index).copied().flatten();
        let bookmark_marker = app.bookmark_marker(index);
        let marker = match app.diagnostic_severity(index) {
            Some(1) => "E",
            Some(2) => "W",
            Some(_) => "I",
            None if bookmark_marker.is_some() => bookmark_marker.as_deref().unwrap_or("◆"),
            None if diff_marker == Some('+') => "+",
            None if app.is_line_folded(index) => "▶",
            None if app.is_line_foldable(index) => "▼",
            None if cursor_positions
                .iter()
                .any(|position| position.line == index && *position != cursor) =>
            {
                "+"
            }
            None => conflict_line_marker(&conflict_blocks, index),
        };
        let (local_cursor_row, local_cursor_column) = if index == cursor.line {
            visual_cursor(line, cursor.column, wrap_width, horizontal_scroll)
        } else {
            (0, 0)
        };
        if index == cursor.line {
            cursor_visual_row = visual_row + local_cursor_row;
            cursor_visual_column = local_cursor_column;
        }
        let chunks = visual_chunks(line, wrap_width, horizontal_scroll, content_width);
        for (chunk_index, chunk) in chunks.into_iter().enumerate() {
            let line_number = if chunk_index == 0 {
                format!("{marker}{:>4} ", index + 1)
            } else {
                "      ".to_owned()
            };
            let background = if selected {
                theme.selection
            } else {
                theme.background
            };
            let foreground = if index == cursor.line {
                theme.accent
            } else {
                theme.foreground
            };
            let selection_modifier = if selected && app.selection_style_inverted() {
                Modifier::REVERSED
            } else {
                Modifier::empty()
            };
            let mut spans = vec![Span::styled(
                line_number,
                Style::default()
                    .fg(if chunk_index == 0 {
                        match marker {
                            "E" | "!" => theme.error,
                            "O" => theme.positive,
                            "T" => theme.warning,
                            "B" => theme.accent,
                            _ => theme.muted,
                        }
                    } else {
                        theme.muted
                    })
                    .add_modifier(selection_modifier),
            )];
            let mut highlighted = highlight_line(&chunk, app.language(), foreground, background);
            for span in &mut highlighted {
                span.style = span.style.add_modifier(selection_modifier);
            }
            spans.extend(highlighted);
            editor_lines.push(Line::from(spans));
            visual_row += 1;
        }
    }
    let plugin_title = app
        .plugin_file_view()
        .map(|(title, _)| format!("Plugin: {title}"));
    let natural_scroll_y =
        cursor_visual_row.saturating_sub(vertical[1].height.saturating_sub(3) as usize);
    let scroll_y = app.editor_scroll_line().unwrap_or(natural_scroll_y);
    let editor = if let Some((_, text)) = app.plugin_file_view() {
        Paragraph::new(text.to_owned())
            .block(panel(
                plugin_title.as_deref().unwrap_or("Plugin"),
                app.focus() == Focus::Editor,
            ))
            .wrap(Wrap { trim: false })
            .scroll((app.plugin_file_view_scroll() as u16, 0))
    } else {
        Paragraph::new(Text::from_iter(editor_lines))
            .block(panel(
                if !app.diff_mode() {
                    "Editor"
                } else {
                    "Editor · Diff"
                },
                app.focus() == Focus::Editor,
            ))
            .wrap(Wrap { trim: false })
            .scroll((scroll_y as u16, 0))
    };
    frame.render_widget(editor, vertical[1]);
    if app.focus() == Focus::Editor && app.plugin_file_view().is_none() {
        let x = vertical[1]
            .x
            .saturating_add(6)
            .saturating_add(1)
            .saturating_add(cursor_visual_column as u16);
        let y = vertical[1]
            .y
            .saturating_add(1)
            .saturating_add(cursor_visual_row.saturating_sub(scroll_y) as u16);
        if x < vertical[1].right() && y < vertical[1].bottom() {
            frame.set_cursor_position((x, y));
        }
    }

    if let Some(popup) = app.lsp_popup() {
        let popup_area = Rect::new(
            vertical[1].x.saturating_add(8),
            vertical[1].y.saturating_add(2),
            vertical[1].width.saturating_sub(10).min(48),
            vertical[1].height.saturating_sub(3).min(8),
        );
        let (title, content) = match popup {
            LspPopup::Completion { items, selected } => (
                "Completion",
                items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        if index == *selected {
                            format!("▶ {item}")
                        } else {
                            format!("  {item}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            LspPopup::CodeActions {
                items, selected, ..
            } => (
                "Code actions",
                items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        if index == *selected {
                            format!("▶ {item}")
                        } else {
                            format!("  {item}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            LspPopup::Locations {
                title,
                locations,
                selected,
            } => (
                title.as_str(),
                locations
                    .iter()
                    .enumerate()
                    .map(|(index, (path, line, column))| {
                        let marker = if index == *selected { "▶" } else { " " };
                        format!("{marker} {}:{}:{}", path.display(), line + 1, column + 1)
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            LspPopup::RenamePreview { summary, .. } => (
                "Rename preview",
                format!(
                    "Press Enter to apply, Esc to cancel\n\n{}",
                    summary.join("\n")
                ),
            ),
            LspPopup::ConflictPreview { lines } => ("Three-way conflict", lines.join("\n")),
            LspPopup::Info(text) => ("Hover", text.clone()),
        };
        frame.render_widget(Clear, popup_area);
        frame.render_widget(
            Paragraph::new(content)
                .style(Style::default().fg(theme.foreground).bg(theme.panel))
                .wrap(Wrap { trim: true })
                .block(panel(title, true)),
            popup_area,
        );
    }
    if app.rename_mode() {
        let popup_area = Rect::new(
            vertical[1].x.saturating_add(8),
            vertical[1].y.saturating_add(2),
            vertical[1].width.saturating_sub(10).min(56),
            3.min(vertical[1].height.saturating_sub(2)),
        );
        frame.render_widget(Clear, popup_area);
        frame.render_widget(
            Paragraph::new(format!("New name: {}", app.rename_text()))
                .style(Style::default().fg(theme.foreground).bg(theme.panel))
                .block(panel("Rename symbol", true)),
            popup_area,
        );
    }

    if !app.bottom_visible() {
        return;
    }
    let bottom = match app.bottom_tab() {
        BottomTab::Terminal => {
            let mut lines = app.terminal_screen_styled_lines();
            if lines.len() > 3 {
                lines = lines.split_off(lines.len() - 3);
            }
            let mut text = Text::default();
            for row in lines {
                text.lines.push(Line::from(
                    row.into_iter()
                        .map(|(character, style)| {
                            Span::styled(character.to_string(), terminal_style(style))
                        })
                        .collect::<Vec<_>>(),
                ));
            }
            text.lines
                .push(Line::from(format!("❯ {}", app.terminal_input())));
            text
        }
        BottomTab::Output => {
            let output = if !app.blame_lines().is_empty() {
                app.blame_lines()
                    .iter()
                    .skip(app.output_scroll())
                    .take(8)
                    .map(|line| {
                        format!(
                            "{} {} {}",
                            &line.commit[..line.commit.len().min(8)],
                            line.author,
                            line.line
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            } else if !app.history().is_empty() {
                app.history()
                    .iter()
                    .skip(app.output_scroll())
                    .take(10)
                    .map(|commit| {
                        format!(
                            "{} {} {} {}",
                            &commit.hash[..commit.hash.len().min(8)],
                            commit.date,
                            commit.author,
                            commit.subject
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            } else if !app.plugin_registry_entries().is_empty() {
                app.plugin_registry_entries()
                    .iter()
                    .skip(app.output_scroll())
                    .take(8)
                    .map(|entry| format!("{} {} · {}", entry.name, entry.version, entry.url))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else if app.change_diff().is_empty() {
                let logs = app.plugin_logs();
                if logs.is_empty() {
                    "[info] output panel ready\n[info] select a Git change to inspect its diff"
                        .to_owned()
                } else {
                    logs.iter()
                        .rev()
                        .skip(app.output_scroll())
                        .take(8)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            } else if !app.pull_request_comments().is_empty() {
                app.pull_request_comments()
                    .iter()
                    .skip(app.output_scroll())
                    .take(8)
                    .map(|comment| {
                        format!(
                            "{}:{} {}: {}",
                            comment.path,
                            comment.line.unwrap_or_default(),
                            comment
                                .user
                                .as_ref()
                                .map_or("unknown", |user| user.login.as_str()),
                            comment.body.replace('\n', " ")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                app.change_diff()
                    .iter()
                    .skip(app.output_scroll())
                    .take(8)
                    .map(|line| {
                        if app.diff_split() {
                            let (left, right) = match line.kind {
                                '-' => (line.text.as_str(), ""),
                                '+' => ("", line.text.as_str()),
                                _ => (line.text.as_str(), line.text.as_str()),
                            };
                            format!("{left:<42} │ {right}")
                        } else {
                            format!("{}{}", line.kind, line.text)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Text::from(output)
        }
    };
    let mut bottom = bottom;
    if app.pull_request_comment_mode() {
        bottom
            .lines
            .push(Line::from(format!("Comment: {}", app.commit_message())));
    }
    if let Some(panel_text) = app.plugin_bottom_panel_text() {
        bottom.lines.push(Line::from(Span::styled(
            "Plugin panel",
            Style::default().fg(theme.positive),
        )));
        bottom
            .lines
            .extend(panel_text.lines().map(|line| Line::from(line.to_owned())));
    }
    let bottom_title = if app.bottom_tab() == BottomTab::Terminal {
        let title = app.terminal_title();
        let terminal_name = if title.is_empty() {
            format!(
                "Terminal {}/{}",
                app.active_terminal() + 1,
                app.terminal_count()
            )
        } else {
            format!(
                "{} ({}/{})",
                title,
                app.active_terminal() + 1,
                app.terminal_count()
            )
        };
        format!("{terminal_name} · Ctrl-Shift-T new · Ctrl-Alt-Left/Right switch")
    } else if app.pull_request_comment_mode() {
        "PR comment · Enter submit · Esc cancel".to_owned()
    } else if app.diff_split() && !app.change_diff().is_empty() {
        "Output · Git diff split · v unified".to_owned()
    } else {
        app.bottom_tab().label().to_owned()
    };
    frame.render_widget(
        Paragraph::new(bottom)
            .style(Style::default().fg(theme.muted))
            .block(panel(
                &bottom_title,
                app.focus() == Focus::Terminal || app.focus() == Focus::Output,
            )),
        vertical[2],
    );
}

fn char_width(character: char) -> usize {
    UnicodeWidthChar::width(character).unwrap_or(0)
}

fn display_column(line: &str, column: usize) -> usize {
    line.chars().take(column).map(char_width).sum()
}

fn visual_cursor(
    line: &str,
    column: usize,
    wrap_width: Option<usize>,
    horizontal_scroll: usize,
) -> (usize, usize) {
    let mut row = 0;
    let mut visual_column = 0;
    let width = wrap_width.unwrap_or(usize::MAX);
    for character in line.chars().take(column) {
        let character_width = char_width(character);
        if character_width > 0 && visual_column + character_width > width {
            row += 1;
            visual_column = 0;
        }
        visual_column += character_width;
        if visual_column == width {
            row += 1;
            visual_column = 0;
        }
    }
    if wrap_width.is_none() {
        (0, visual_column.saturating_sub(horizontal_scroll))
    } else {
        (row, visual_column)
    }
}

fn visual_chunks(
    line: &str,
    wrap_width: Option<usize>,
    horizontal_scroll: usize,
    content_width: usize,
) -> Vec<String> {
    let Some(width) = wrap_width else {
        let mut result = String::new();
        let mut display_column = 0;
        for character in line.chars() {
            let character_width = char_width(character);
            if display_column + character_width <= horizontal_scroll {
                display_column += character_width;
                continue;
            }
            if display_column >= horizontal_scroll.saturating_add(content_width) {
                break;
            }
            result.push(character);
            display_column += character_width;
        }
        return vec![result];
    };
    let width = width.max(1);
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    let mut chunk_width = 0;
    for character in line.chars() {
        let character_width = char_width(character);
        if character_width > 0 && chunk_width + character_width > width && !chunk.is_empty() {
            chunks.push(std::mem::take(&mut chunk));
            chunk_width = 0;
        }
        chunk.push(character);
        chunk_width += character_width;
        if chunk_width >= width {
            chunks.push(std::mem::take(&mut chunk));
            chunk_width = 0;
        }
    }
    if !chunk.is_empty() || chunks.is_empty() || chunk_width == 0 {
        chunks.push(chunk);
    }
    chunks
}

fn highlight_line(
    text: &str,
    language: &str,
    foreground: Color,
    background: Color,
) -> Vec<Span<'static>> {
    let theme = active_theme();
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if let Some(prefix) = comment_prefix(language) {
            let marker: Vec<char> = prefix.chars().collect();
            if chars[index..].starts_with(&marker) {
                spans.push(Span::styled(
                    chars[index..].iter().collect::<String>(),
                    Style::default().fg(theme.muted).bg(background),
                ));
                break;
            }
        }
        let character = chars[index];
        if matches!(character, '"' | '\'' | '`') {
            let quote = character;
            let start = index;
            index += 1;
            while index < chars.len() {
                if chars[index] == quote && chars.get(index.saturating_sub(1)) != Some(&'\\') {
                    index += 1;
                    break;
                }
                index += 1;
            }
            spans.push(Span::styled(
                chars[start..index].iter().collect::<String>(),
                Style::default().fg(theme.warning).bg(background),
            ));
        } else if character.is_ascii_digit() {
            let start = index;
            index += 1;
            while index < chars.len() && (chars[index].is_ascii_digit() || chars[index] == '.') {
                index += 1;
            }
            spans.push(Span::styled(
                chars[start..index].iter().collect::<String>(),
                Style::default().fg(theme.warning).bg(background),
            ));
        } else if character.is_ascii_alphabetic() || character == '_' {
            let start = index;
            index += 1;
            while index < chars.len()
                && (chars[index].is_ascii_alphanumeric() || chars[index] == '_')
            {
                index += 1;
            }
            let word: String = chars[start..index].iter().collect();
            let color = if is_keyword(language, &word) {
                theme.accent
            } else {
                foreground
            };
            spans.push(Span::styled(
                word,
                Style::default().fg(color).bg(background),
            ));
        } else {
            spans.push(Span::styled(
                character.to_string(),
                Style::default().fg(foreground).bg(background),
            ));
            index += 1;
        }
    }
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), Style::default().bg(background)));
    }
    spans
}

fn comment_prefix(language: &str) -> Option<&'static str> {
    match language {
        "py" | "pyi" | "sh" | "bash" | "zsh" | "yaml" | "yml" | "toml" => Some("#"),
        "lua" => Some("--"),
        "rs" | "go" | "js" | "jsx" | "ts" | "tsx" | "java" | "c" | "cpp" | "h" => Some("//"),
        _ => None,
    }
}

fn is_keyword(language: &str, word: &str) -> bool {
    let common = matches!(
        word,
        "if" | "else"
            | "for"
            | "while"
            | "return"
            | "match"
            | "break"
            | "continue"
            | "true"
            | "false"
            | "null"
            | "nil"
    );
    let language_keyword = match language {
        "rs" => matches!(
            word,
            "fn" | "let"
                | "mut"
                | "struct"
                | "enum"
                | "impl"
                | "trait"
                | "pub"
                | "use"
                | "mod"
                | "crate"
                | "self"
                | "async"
                | "await"
        ),
        "go" => matches!(
            word,
            "func"
                | "var"
                | "const"
                | "type"
                | "struct"
                | "package"
                | "import"
                | "go"
                | "defer"
                | "chan"
                | "range"
        ),
        "py" | "pyi" => matches!(
            word,
            "def"
                | "class"
                | "import"
                | "from"
                | "as"
                | "in"
                | "with"
                | "lambda"
                | "yield"
                | "True"
                | "False"
                | "None"
        ),
        "js" | "jsx" | "ts" | "tsx" => matches!(
            word,
            "function"
                | "const"
                | "let"
                | "var"
                | "class"
                | "new"
                | "import"
                | "from"
                | "export"
                | "async"
                | "await"
                | "typeof"
        ),
        "lua" => matches!(
            word,
            "local" | "function" | "end" | "then" | "do" | "require" | "return" | "and" | "or"
        ),
        _ => false,
    };
    common || language_keyword
}

fn terminal_style(style: TerminalStyle) -> Style {
    let mut foreground = style.foreground.map(ansi_color);
    let mut background = style.background.map(ansi_color);
    if style.inverse {
        std::mem::swap(&mut foreground, &mut background);
    }
    if style.conceal {
        foreground = Some(background.unwrap_or(Color::Reset));
    }
    let mut result = Style::default().fg(foreground.unwrap_or(Color::White));
    if let Some(background) = background {
        result = result.bg(background);
    }
    if style.bold {
        result = result.add_modifier(Modifier::BOLD);
    }
    if style.dim {
        result = result.add_modifier(Modifier::DIM);
    }
    if style.italic {
        result = result.add_modifier(Modifier::ITALIC);
    }
    if style.strikethrough {
        result = result.add_modifier(Modifier::CROSSED_OUT);
    }
    if style.blink {
        result = result.add_modifier(Modifier::SLOW_BLINK);
    }
    if style.underline {
        result = result.add_modifier(Modifier::UNDERLINED);
    }
    if matches!(
        style.cursor_shape,
        Some(CursorShape::Underline | CursorShape::Bar)
    ) {
        result = result.add_modifier(Modifier::UNDERLINED);
    }
    result
}

fn ansi_color(color: AnsiColor) -> Color {
    match color {
        AnsiColor::Basic(index) => basic_color(index, false),
        AnsiColor::Bright(index) => basic_color(index, true),
        AnsiColor::Indexed(index) => Color::Indexed(index),
        AnsiColor::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

fn basic_color(index: u8, bright: bool) -> Color {
    let colors = if bright {
        [
            Color::Gray,
            Color::LightRed,
            Color::LightGreen,
            Color::LightYellow,
            Color::LightBlue,
            Color::LightMagenta,
            Color::LightCyan,
            Color::White,
        ]
    } else {
        [
            Color::Black,
            Color::Red,
            Color::Green,
            Color::Yellow,
            Color::Blue,
            Color::Magenta,
            Color::Cyan,
            Color::Gray,
        ]
    };
    colors[index.min(7) as usize]
}

fn draw_agent(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = active_theme();
    if app.agent_install_confirm() {
        let command = app.agent_install_command().unwrap_or("(missing)");
        let content = Paragraph::new(format!(
            "Install {}?\n\n{}\n\nEnter confirm · Esc cancel",
            app.agent_name(),
            command
        ))
        .style(Style::default().fg(theme.warning))
        .wrap(Wrap { trim: false })
        .block(panel("AI Agent installation", true));
        frame.render_widget(content, area);
        return;
    }
    let mut lines = app.agent_screen_lines();
    if lines.len() > 14 {
        lines = lines.split_off(lines.len() - 14);
    }
    lines.insert(0, format!("{}  [ / ] switch", app.agent_name()));
    lines.push(format!("❯ {}", app.agent_input()));
    let content = if app.agent_configured() && app.agent_executable_available() {
        Paragraph::new(lines.join("\n"))
    } else {
        Paragraph::new(format!(
            "{}\n\nAgent unavailable\n\nPress i to review installation\n\n[ / ] switch provider",
            app.agent_name()
        ))
    }
    .style(Style::default().fg(theme.muted))
    .wrap(Wrap { trim: false })
    .block(panel("AI Agent", app.focus() == Focus::Agent));
    frame.render_widget(content, area);
}

fn draw_plugin_palette(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = active_theme();
    let width = area.width.clamp(30, 70);
    let height = area.height.clamp(5, 14);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 3,
        width,
        height,
    );
    let items = app.plugin_palette_items();
    let mut lines = vec![format!("Command palette · {}", app.plugin_palette_query())];
    if items.is_empty() {
        lines.push("No commands".to_owned());
    } else {
        lines.extend(
            items
                .iter()
                .enumerate()
                .take(height.saturating_sub(3) as usize)
                .map(|(index, (id, title))| {
                    let marker = if index == app.plugin_palette_index() {
                        "▶"
                    } else {
                        " "
                    };
                    format!("{marker} {title}  ({id})")
                }),
        );
    }
    lines.push("Enter execute · Esc close".to_owned());
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines.join("\n"))
            .style(Style::default().fg(theme.foreground).bg(theme.panel))
            .block(panel("Command Palette", true))
            .wrap(Wrap { trim: true }),
        popup,
    );
}

fn draw_settings_overlay(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = active_theme();
    let width = area.width.clamp(34, 64);
    let height = area.height.clamp(8, 14);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 3,
        width,
        height,
    );
    let rows = app.settings_rows();
    let mut lines = vec!["↑/↓ select · Enter/Space toggle · Ctrl-S save · Esc close".to_owned()];
    lines.extend(rows.iter().enumerate().map(|(index, (label, enabled))| {
        let marker = if index == app.settings_index() {
            "▶"
        } else {
            " "
        };
        let value = if *enabled { "on" } else { "off" };
        format!("{marker} {label:<24} {value}")
    }));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines.join("\n"))
            .style(Style::default().fg(theme.foreground).bg(theme.panel))
            .block(panel("Settings · session values", true))
            .wrap(Wrap { trim: true }),
        popup,
    );
}

fn draw_quick_open(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = active_theme();
    let width = area.width.clamp(32, 72);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + 2,
        width,
        3.min(area.height.saturating_sub(2)),
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!("Path: {}", app.quick_open_query()))
            .style(Style::default().fg(theme.foreground).bg(theme.panel))
            .block(panel(app.quick_open_title(), true)),
        popup,
    );
}

fn draw_explorer_input(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = active_theme();
    let width = area.width.clamp(36, 72);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + 3,
        width,
        3.min(area.height.saturating_sub(2)),
    );
    let title = app.explorer_mode().unwrap_or("Explorer");
    let value = if title == "Confirm delete" {
        app.status_message().to_owned()
    } else {
        format!("Name: {}", app.explorer_input())
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(value)
            .style(Style::default().fg(theme.foreground).bg(theme.panel))
            .block(panel(title, true)),
        popup,
    );
}

fn draw_goto(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = active_theme();
    let width = area.width.clamp(30, 52);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + 2,
        width,
        3.min(area.height.saturating_sub(2)),
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!("Line: {}", app.goto_input()))
            .style(Style::default().fg(theme.foreground).bg(theme.panel))
            .block(panel("Go to Line", true)),
        popup,
    );
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = active_theme();
    let cursor = app.cursor();
    let diagnostic = app
        .diagnostic_message(cursor.line)
        .map(|message| format!(" · {message}"))
        .unwrap_or_default();
    let (plugin_left, plugin_right) = app.plugin_status_items_by_side();
    let plugin_left = plugin_left.join(" · ");
    let plugin_right = plugin_right.join(" · ");
    let plugin_left = if plugin_left.is_empty() {
        String::new()
    } else {
        format!(" · {plugin_left}")
    };
    let plugin_right = if plugin_right.is_empty() {
        String::new()
    } else {
        format!(" · {plugin_right}")
    };
    let text = format!(
        " {}{}{}   Ln {}, Col {}   UTF-8   {}   {}   {}{}   Tab: focus · Ctrl+Q: quit",
        app.status_message(),
        diagnostic,
        plugin_left,
        cursor.line + 1,
        cursor.column + 1,
        app.language(),
        app.focus().label(),
        app.branch().map_or("Git:—", |branch| branch),
        plugin_right,
    );
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(theme.background).bg(theme.accent)),
        area,
    );
}

fn draw_small(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let theme = active_theme();
    frame.render_widget(Clear, area);
    let content = format!(
        "mtide\n\n{}\n\nCtrl+Q quit · Tab focus",
        app.status_message()
    );
    frame.render_widget(
        Paragraph::new(content)
            .style(Style::default().fg(theme.foreground).bg(theme.background))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.accent)),
            ),
        area,
    );
}

fn panel<'a>(title: &'a str, focused: bool) -> Block<'a> {
    let theme = active_theme();
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .style(Style::default().fg(theme.foreground).bg(theme.panel))
        .border_style(Style::default().fg(if focused { theme.accent } else { theme.border }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_wide_characters_by_display_columns() {
        assert_eq!(
            visual_chunks("你好世界", Some(4), 0, 20),
            vec!["你好", "世界", ""]
        );
        assert_eq!(visual_cursor("你好世界", 1, Some(4), 0), (0, 2));
        assert_eq!(visual_cursor("你好世界", 2, Some(4), 0), (1, 0));
    }

    #[test]
    fn horizontal_viewport_uses_display_columns() {
        assert_eq!(visual_chunks("a你好b", None, 2, 4), vec!["你好b"]);
        assert_eq!(visual_cursor("a你好b", 2, None, 2), (0, 1));
    }

    #[test]
    fn conflict_lines_receive_three_way_markers() {
        let lines = vec![
            "<<<<<<< ours".to_owned(),
            "ours".to_owned(),
            "||||||| base".to_owned(),
            "base".to_owned(),
            "=======".to_owned(),
            "theirs".to_owned(),
            ">>>>>>> theirs".to_owned(),
        ];
        let blocks = crate::git::conflict_blocks(&lines);
        assert_eq!(conflict_line_marker(&blocks, 0), "!");
        assert_eq!(conflict_line_marker(&blocks, 1), "O");
        assert_eq!(conflict_line_marker(&blocks, 3), "B");
        assert_eq!(conflict_line_marker(&blocks, 5), "T");
        assert_eq!(conflict_line_marker(&blocks, 6), "!");
    }
}
