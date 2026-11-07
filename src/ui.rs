use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Tabs, Wrap},
    Frame,
};
use std::sync::{OnceLock, RwLock};

use crate::app::{App, BottomTab, ChangeTreeRowKind, Focus, LspPopup, PluginDialog, SidebarTab};
use crate::config::Theme;
use crate::git::ConflictBlock;
use crate::lsp::LspServerState;
use crate::plugin::PluginPanelLocation;
use crate::terminal::{AnsiColor, CursorShape, TerminalStyle};
use regex::Regex;
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
        draw_overlays(frame, area, app);
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
    draw_overlays(frame, area, app);
}

fn draw_overlays(frame: &mut Frame<'_>, area: Rect, app: &App) {
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
    if app.plugin_context_menu().is_some() {
        draw_plugin_context_menu(frame, area, app);
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

fn plugin_context_menu_rect(area: Rect, app: &App) -> Option<Rect> {
    let menu = app.plugin_context_menu()?;
    let max_label_width = menu
        .items
        .iter()
        .map(|item| {
            item.chars()
                .map(|ch| ch.width().unwrap_or(1))
                .sum::<usize>()
        })
        .max()
        .unwrap_or(10);
    let requested_width = (max_label_width as u16).saturating_add(6);
    let width = requested_width.min(area.width.saturating_sub(2)).max(1);
    let requested_height = (menu.items.len() as u16).saturating_add(2);
    let height = requested_height.min(area.height.saturating_sub(2)).max(1);
    Some(Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    ))
}

fn draw_plugin_context_menu(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = active_theme();
    let Some(menu) = app.plugin_context_menu() else {
        return;
    };
    let Some(popup) = plugin_context_menu_rect(area, app) else {
        return;
    };
    let lines = menu
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let marker = if index == menu.selected { "▶" } else { " " };
            format!("{marker} {item}")
        })
        .collect::<Vec<_>>();
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines.join("\n"))
            .style(Style::default().fg(theme.foreground).bg(theme.panel))
            .block(panel("Plugin Context Menu", true))
            .wrap(Wrap { trim: true }),
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
    let Some((side, requested_width, min_width, _)) = app.plugin_drawer() else {
        return;
    };
    let Some(drawer) = plugin_drawer_rect(area, side, requested_width, min_width) else {
        return;
    };
    let text = app
        .plugin_drawer_text_sized(
            drawer.width.saturating_sub(2),
            drawer.height.saturating_sub(2),
        )
        .unwrap_or_default();
    frame.render_widget(Clear, drawer);
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(theme.foreground).bg(theme.panel))
            .wrap(Wrap { trim: false })
            .block(panel("Plugin Drawer", true)),
        drawer,
    );
}

fn plugin_drawer_rect(
    area: Rect,
    side: &str,
    requested_width: u16,
    min_width: u16,
) -> Option<Rect> {
    let max_width = area.width.saturating_sub(4);
    if max_width == 0 || area.height < 4 {
        return None;
    }
    let width = requested_width.max(min_width).min(max_width).max(1);
    Some(Rect::new(
        if side == "left" {
            1
        } else {
            area.right().saturating_sub(width + 1)
        },
        1,
        width,
        area.height.saturating_sub(2),
    ))
}

pub fn handle_mouse(app: &mut App, event: MouseEvent, area: Rect) {
    if app.plugin_context_menu().is_some() {
        let Some(popup) = plugin_context_menu_rect(area, app) else {
            return;
        };
        let inside = event.column >= popup.x
            && event.column < popup.right()
            && event.row >= popup.y
            && event.row < popup.bottom();
        let selected_line =
            inside.then(|| event.row.saturating_sub(popup.y.saturating_add(1)) as usize);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if inside => {
                if let Some(line) = selected_line {
                    if let Err(error) = app.plugin_context_menu_click(line) {
                        app.set_status_message(format!(
                            "Plugin context menu click failed: {error}"
                        ));
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Down(MouseButton::Right) => {
                app.close_plugin_context_menu()
            }
            _ => {}
        }
        return;
    }
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
    let drawer_hit = app
        .plugin_drawer()
        .and_then(|(side, requested_width, min_width, _)| {
            plugin_drawer_rect(area, side, requested_width, min_width)
        });
    if let Some(drawer) = drawer_hit {
        let inside = event.column >= drawer.x
            && event.column < drawer.right()
            && event.row >= drawer.y
            && event.row < drawer.bottom();
        if inside {
            if matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) {
                let line = event.row.saturating_sub(drawer.y.saturating_add(1)) as usize;
                if !app.focus_plugin_drawer_line(line) {
                    if let Err(error) = app.activate_plugin_drawer_line(line) {
                        app.set_status_message(format!("Plugin drawer click failed: {error}"));
                    }
                }
            }
            return;
        }
    }
    if matches!(event.kind, MouseEventKind::Down(MouseButton::Left))
        && event.row == area.bottom().saturating_sub(1)
    {
        match app.activate_plugin_status_at(event.column) {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                app.set_status_message(format!("Plugin status click failed: {error}"));
                return;
            }
        }
    }
    if let Some(index) = menu_hit(area, event.column, event.row) {
        if matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) {
            app.menu_click(index);
        }
        return;
    }
    if app.bottom_visible() && !app.terminal_fullscreen() {
        if app.bottom_resizing() {
            match event.kind {
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Moved => {
                    app.set_bottom_height(
                        area.bottom().saturating_sub(event.row.saturating_add(1)),
                    );
                }
                MouseEventKind::Up(MouseButton::Left) => app.end_bottom_resize(),
                _ => {}
            }
            return;
        }
        if let Some(bottom) = terminal_area(app, area) {
            let near_divider = event.row.abs_diff(bottom.y) <= 1;
            let inside_center = event.column >= bottom.x && event.column < bottom.right();
            if near_divider
                && inside_center
                && matches!(event.kind, MouseEventKind::Down(MouseButton::Left))
            {
                app.begin_bottom_resize(area.bottom().saturating_sub(event.row.saturating_add(1)));
                return;
            }
        }
    }
    if app.sidebar_visible() && !app.terminal_fullscreen() {
        if app.sidebar_resizing() {
            match event.kind {
                MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Moved => {
                    app.resize_sidebar_to(
                        event.column.saturating_sub(area.x),
                        area.width.saturating_sub(28),
                    );
                }
                MouseEventKind::Up(MouseButton::Left) => app.end_sidebar_resize(),
                _ => {}
            }
            return;
        }
        if let Some(sidebar) = sidebar_area(app, area) {
            let divider = sidebar.right();
            let near_divider = event.column.abs_diff(divider) <= 1;
            if near_divider
                && event.row >= sidebar.y
                && event.row < sidebar.bottom()
                && matches!(event.kind, MouseEventKind::Down(MouseButton::Left))
            {
                app.begin_sidebar_resize(
                    event.column.saturating_sub(sidebar.x),
                    area.width.saturating_sub(28),
                );
                return;
            }
        }
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
            let body_line = event.row.saturating_sub(body.y.saturating_add(1)) as usize;
            if app.sidebar_tab() != SidebarTab::Explorer {
                let button = mouse_event_button(event.kind);
                let x = event.column.saturating_sub(body.x.saturating_add(1));
                let y = body_line as u16;
                if matches!(event.kind, MouseEventKind::Down(MouseButton::Right)) {
                    match app.open_plugin_panel_context_menu(PluginPanelLocation::Sidebar, x, y) {
                        Ok(true) => return,
                        Ok(false) => {}
                        Err(error) => app.set_status_message(format!(
                            "Plugin sidebar context menu failed: {error}"
                        )),
                    }
                }
                match app.emit_plugin_panel_mouse(PluginPanelLocation::Sidebar, x, y, button) {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(error) => {
                        app.set_status_message(format!("Plugin sidebar mouse failed: {error}"))
                    }
                }
            }
            if matches!(event.kind, MouseEventKind::Down(MouseButton::Left))
                && app.sidebar_tab() != SidebarTab::Explorer
            {
                if let Some(start) = app.plugin_panel_line_start(PluginPanelLocation::Sidebar) {
                    if body_line >= start {
                        match app.activate_plugin_panel_selection(
                            PluginPanelLocation::Sidebar,
                            body_line.saturating_sub(start),
                        ) {
                            Ok(true) => return,
                            Ok(false) => {}
                            Err(error) => {
                                app.set_status_message(format!(
                                    "Plugin sidebar selection failed: {error}"
                                ));
                            }
                        }
                    }
                }
            }
            if event.row < body.y {
                if !matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) {
                    return;
                }
                let tab_width = usize::from(sidebar.width.max(1)) / 4;
                let index = usize::from(event.column.saturating_sub(sidebar.x)) / tab_width.max(1);
                app.select_sidebar_tab_by_index(index.min(4));
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
            } else if app.sidebar_tab() == SidebarTab::Changes
                && event.row >= body.y.saturating_add(2)
            {
                let index = usize::from(event.row.saturating_sub(body.y).saturating_sub(2));
                match event.kind {
                    MouseEventKind::Moved => app.changes_tree_hover(index),
                    MouseEventKind::Down(MouseButton::Left) => {
                        app.activate_changes_tree_row(index);
                    }
                    _ => {}
                }
            } else if app.sidebar_tab() == SidebarTab::Problems
                && event.row >= body.y.saturating_add(2)
            {
                let index = usize::from(event.row.saturating_sub(body.y).saturating_sub(2));
                app.set_problem_index(index);
                if matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) {
                    app.activate_problem(index);
                }
            } else if app.sidebar_tab() == SidebarTab::Search
                && event.row >= body.y.saturating_add(1)
                && matches!(event.kind, MouseEventKind::Down(MouseButton::Left))
            {
                let field_row = event.row.saturating_sub(body.y).saturating_sub(1);
                let field = field_row as u8;
                if field < 3 {
                    let label_width = [9u16, 10, 10][field as usize];
                    let cursor = usize::from(
                        event
                            .column
                            .saturating_sub(body.x)
                            .saturating_sub(1 + label_width),
                    );
                    app.set_search_field(field, cursor);
                } else if field == 4 && app.replace_mode() {
                    let cursor =
                        usize::from(event.column.saturating_sub(body.x).saturating_sub(10));
                    app.set_replace_focus(cursor);
                }
            }
            return;
        }
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
            if app.bottom_tab() == crate::app::BottomTab::Output {
                let button = mouse_event_button(event.kind);
                let x = event.column.saturating_sub(bottom.x.saturating_add(1));
                let y = event.row.saturating_sub(bottom.y.saturating_add(1));
                if matches!(event.kind, MouseEventKind::Down(MouseButton::Right)) {
                    match app.open_plugin_panel_context_menu(PluginPanelLocation::Bottom, x, y) {
                        Ok(true) => return,
                        Ok(false) => {}
                        Err(error) => app.set_status_message(format!(
                            "Plugin bottom context menu failed: {error}"
                        )),
                    }
                }
                match app.emit_plugin_panel_mouse(PluginPanelLocation::Bottom, x, y, button) {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(error) => {
                        app.set_status_message(format!("Plugin bottom mouse failed: {error}"))
                    }
                }
                if matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) {
                    let line = event.row.saturating_sub(bottom.y.saturating_add(1)) as usize;
                    if let Some(start) = app.plugin_panel_line_start(PluginPanelLocation::Bottom) {
                        if line >= start {
                            match app.activate_plugin_panel_selection(
                                PluginPanelLocation::Bottom,
                                line.saturating_sub(start),
                            ) {
                                Ok(true) => return,
                                Ok(false) => {}
                                Err(error) => {
                                    app.set_status_message(format!(
                                        "Plugin bottom selection failed: {error}"
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            app.set_focus(if app.bottom_tab() == crate::app::BottomTab::Output {
                Focus::Output
            } else {
                Focus::Terminal
            });
            if app.bottom_tab() == BottomTab::Output && app.output_reference_lines().is_some() {
                let content_left = bottom.x.saturating_add(1);
                let content_top = bottom.y.saturating_add(1);
                if event.column >= content_left && event.row >= content_top {
                    let column = usize::from(event.column.saturating_sub(content_left));
                    let row = usize::from(event.row.saturating_sub(content_top));
                    match event.kind {
                        MouseEventKind::Moved => app.update_output_hover(
                            row,
                            column,
                            event
                                .modifiers
                                .contains(crossterm::event::KeyModifiers::CONTROL),
                        ),
                        MouseEventKind::Down(MouseButton::Left) => {
                            if event
                                .modifiers
                                .contains(crossterm::event::KeyModifiers::CONTROL)
                                && app.open_output_link_at(row, column)
                            {
                                return;
                            }
                            app.update_output_hover(row, column, false);
                        }
                        _ => {}
                    }
                }
            }
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
        if app.terminal_mouse_tracking() {
            if let Err(error) = app.send_terminal_mouse(column, row, event.kind, event.modifiers) {
                app.set_status_message(format!("Terminal mouse error: {error}"));
            }
            return;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => app.begin_terminal_selection(column, row),
            MouseEventKind::Drag(MouseButton::Left) => app.update_terminal_selection(column, row),
            MouseEventKind::Up(MouseButton::Left) => {
                let is_empty = app
                    .terminal_selection_text()
                    .is_none_or(|text| text.is_empty());
                app.finish_terminal_selection();
                if is_empty {
                    if let Some(url) = app.terminal_link_at(column, row) {
                        app.open_terminal_link(&url);
                    }
                }
            }
            MouseEventKind::ScrollUp => app.scroll_terminal(3),
            MouseEventKind::ScrollDown => app.scroll_terminal(-3),
            _ => {}
        }
        return;
    }
    if (app.plugin_custom_tab_active() || app.plugin_file_view().is_some())
        && app.focus() == Focus::Editor
    {
        if let Some(editor) = editor_content_area(app, area) {
            if event.column >= editor.x
                && event.column < editor.right()
                && event.row >= editor.y
                && event.row < editor.bottom()
            {
                let x = event.column.saturating_sub(editor.x);
                let y = event.row.saturating_sub(editor.y);
                let button = mouse_event_button(event.kind);
                let handled = if app.plugin_custom_tab_active() {
                    app.emit_plugin_tab_mouse(x, y, button)
                } else {
                    app.emit_plugin_file_mouse(x, y, button)
                };
                match handled {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(error) => {
                        app.set_status_message(format!("Plugin tab mouse failed: {error}"))
                    }
                }
            }
        }
    }
    if app.plugin_diff_view_active() && app.focus() == Focus::Editor {
        if let Some(editor) = editor_content_area(app, area) {
            let content_left = editor.x.saturating_add(1);
            let content_top = editor.y.saturating_add(1);
            let content_width = editor.width.saturating_sub(2);
            let content_height = editor.height.saturating_sub(2);
            if event.column >= content_left
                && event.column < content_left.saturating_add(content_width)
                && event.row >= content_top
                && event.row < content_top.saturating_add(content_height)
            {
                let x = usize::from(event.column.saturating_sub(content_left));
                let y = usize::from(event.row.saturating_sub(content_top));
                let width = content_width.max(1);
                let height = content_height.max(1);
                match event.kind {
                    MouseEventKind::Moved => app.update_plugin_file_view_hover(
                        y,
                        x,
                        width,
                        height,
                        event
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL),
                    ),
                    MouseEventKind::Down(MouseButton::Left) => {
                        if event
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL)
                            && app.open_plugin_file_view_link_at(y, x, width, height)
                        {
                            return;
                        }
                        app.update_plugin_file_view_hover(y, x, width, height, false);
                    }
                    MouseEventKind::ScrollUp => app.scroll_plugin_file_view(-3),
                    MouseEventKind::ScrollDown => app.scroll_plugin_file_view(3),
                    _ => {}
                }
                return;
            }
        }
    }
    if matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) {
        if let Some(line) = editor_gutter_line(app, area, event.column, event.row) {
            match app.emit_plugin_gutter_click(line) {
                Ok(true) => return,
                Ok(false) => {}
                Err(error) => {
                    app.set_status_message(format!("Plugin gutter event failed: {error}"))
                }
            }
        }
    }
    let position = editor_mouse_position(app, area, event.column, event.row);
    if matches!(event.kind, MouseEventKind::Moved) {
        app.update_editor_hover(
            position,
            event
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL),
        );
    }
    match event.kind {
        MouseEventKind::ScrollUp => {
            if event.modifiers.intersects(
                crossterm::event::KeyModifiers::SHIFT | crossterm::event::KeyModifiers::ALT,
            ) {
                app.scroll_editor_horizontal(-4);
            } else {
                app.scroll_editor(-3);
            }
        }
        MouseEventKind::ScrollDown => {
            if event.modifiers.intersects(
                crossterm::event::KeyModifiers::SHIFT | crossterm::event::KeyModifiers::ALT,
            ) {
                app.scroll_editor_horizontal(4);
            } else {
                app.scroll_editor(3);
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(position) = position {
                if event
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL)
                    && app.open_editor_link(position)
                {
                    return;
                }
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
        MouseEventKind::Down(MouseButton::Right) => {
            if let Some(position) = position {
                if let Err(error) = app.open_plugin_context_menu(position) {
                    app.set_status_message(format!("Plugin context menu failed: {error}"));
                }
            }
        }
        _ => {}
    }
}

fn mouse_event_button(kind: MouseEventKind) -> &'static str {
    match kind {
        MouseEventKind::Down(MouseButton::Left)
        | MouseEventKind::Up(MouseButton::Left)
        | MouseEventKind::Drag(MouseButton::Left) => "left",
        MouseEventKind::Down(MouseButton::Right)
        | MouseEventKind::Up(MouseButton::Right)
        | MouseEventKind::Drag(MouseButton::Right) => "right",
        MouseEventKind::Down(MouseButton::Middle)
        | MouseEventKind::Up(MouseButton::Middle)
        | MouseEventKind::Drag(MouseButton::Middle) => "middle",
        MouseEventKind::ScrollUp => "wheel_up",
        MouseEventKind::ScrollDown => "wheel_down",
        MouseEventKind::ScrollLeft => "wheel_left",
        MouseEventKind::ScrollRight => "wheel_right",
        MouseEventKind::Moved => "none",
    }
}

fn menu_hit(area: Rect, column: u16, row: u16) -> Option<usize> {
    if area.width < 50 || area.height < 10 || row != area.y {
        return None;
    }
    let offset = column.checked_sub(area.x.saturating_add(6))?;
    if offset >= 45 {
        return None;
    }
    Some(match offset {
        0..=6 => 0,
        7..=12 => 1,
        13..=23 => 2,
        24..=29 => 3,
        30..=38 => 4,
        _ => 5,
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
    if app.terminal_fullscreen() {
        return Some(main);
    }
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
                Constraint::Length(app.bottom_height()),
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
    let editor = editor_content_area(app, area)?;
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

fn editor_content_area(app: &App, area: Rect) -> Option<Rect> {
    if area.width < 50 || area.height < 10 || app.terminal_fullscreen() {
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
            Constraint::Length(app.bottom_height()),
        ]
    } else {
        vec![Constraint::Length(2), Constraint::Min(3)]
    };
    Layout::default()
        .direction(Direction::Vertical)
        .constraints(center_constraints)
        .split(center)
        .get(1)
        .copied()
}

fn editor_gutter_line(app: &App, area: Rect, column: u16, row: u16) -> Option<usize> {
    let editor = editor_content_area(app, area)?;
    if column != editor.x || row <= editor.y || row >= editor.bottom() {
        return None;
    }
    let scroll_y = app
        .cursor()
        .line
        .saturating_sub(editor.height.saturating_sub(3) as usize);
    Some(
        (scroll_y + row.saturating_sub(editor.y + 1) as usize)
            .min(app.file_lines().len().saturating_sub(1)),
    )
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
            "  File  Edit  Selection  View  Options  Help ",
            Style::default().fg(theme.muted),
        ),
        Span::styled(
            format!("  {}  ", app.project_name()),
            Style::default()
                .fg(theme.foreground)
                .add_modifier(Modifier::BOLD),
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
    app.set_plugin_panel_line_start(PluginPanelLocation::Sidebar, None);
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
        SidebarTab::Problems.label(),
    ])
    .select(match app.sidebar_tab() {
        SidebarTab::Explorer => 0,
        SidebarTab::Search => 1,
        SidebarTab::Changes => 2,
        SidebarTab::Branches => 3,
        SidebarTab::Problems => 4,
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
        SidebarTab::Problems => Vec::new(),
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
        let field_marker = |field: u8| {
            if app.search_field() == field {
                "▶"
            } else {
                " "
            }
        };
        let input_line = |marker: &str, label: &str, value: &str, field: u8| {
            let prefix = format!("{marker} {label}: ");
            let active = app.focus() == Focus::Search
                && app.search_field() == field
                && !(field == 0 && app.replace_mode());
            if active {
                let cursor = app.search_input_cursor().min(value.chars().count());
                let before: String = value.chars().take(cursor).collect();
                let after: String = value.chars().skip(cursor).collect();
                Line::from(vec![
                    Span::styled(prefix, Style::default().fg(theme.foreground)),
                    Span::styled(before, Style::default().fg(theme.foreground)),
                    Span::styled("▏", Style::default().fg(theme.accent)),
                    Span::styled(after, Style::default().fg(theme.foreground)),
                ])
            } else {
                Line::from(Span::styled(
                    format!("{prefix}{value}"),
                    Style::default().fg(if app.search_field() == field {
                        theme.foreground
                    } else {
                        theme.muted
                    }),
                ))
            }
        };
        sidebar_text
            .lines
            .push(input_line(field_marker(0), "Search", app.search_query(), 0));
        sidebar_text.lines.push(input_line(
            field_marker(1),
            "Include",
            app.search_include(),
            1,
        ));
        sidebar_text.lines.push(input_line(
            field_marker(2),
            "Exclude",
            app.search_exclude(),
            2,
        ));
        sidebar_text.lines.push(Line::from(Span::styled(
            if app.search_regex() {
                "Tab/Shift-Tab fields · Regex"
            } else {
                "Tab/Shift-Tab fields · Smart-case"
            },
            Style::default().fg(theme.muted),
        )));
        if app.replace_mode() {
            let value = app.replace_query();
            let cursor = app.search_input_cursor().min(value.chars().count());
            let before: String = value.chars().take(cursor).collect();
            let after: String = value.chars().skip(cursor).collect();
            sidebar_text.lines.push(Line::from(vec![
                Span::styled("Replace: ", Style::default().fg(theme.warning)),
                Span::styled(before, Style::default().fg(theme.warning)),
                Span::styled(
                    if app.focus() == Focus::Search && app.search_field() == 0 {
                        "▏"
                    } else {
                        ""
                    },
                    Style::default().fg(theme.accent),
                ),
                Span::styled(after, Style::default().fg(theme.warning)),
            ]));
            sidebar_text.lines.push(Line::from(Span::styled(
                "Ctrl-Enter: one · Enter: current file · Ctrl-Shift-Enter: workspace",
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
                match app.explorer_git_status(path) {
                    Some(('?', '?')) | Some(('A', _)) | Some(('C', _)) => {
                        Style::default().fg(theme.positive)
                    }
                    Some(('D', _)) | Some((_, 'D')) | Some(('U', _)) | Some((_, 'U')) => {
                        Style::default().fg(theme.error)
                    }
                    Some(('M', _)) | Some((_, 'M')) => Style::default().fg(theme.warning),
                    Some((index_status, worktree_status))
                        if index_status != ' ' || worktree_status != ' ' =>
                    {
                        Style::default().fg(theme.accent)
                    }
                    _ => Style::default().fg(theme.foreground),
                }
            };
            let status = app
                .explorer_git_status(path)
                .map_or("  ".to_owned(), |(index_status, worktree_status)| {
                    format!("{index_status}{worktree_status}")
                });
            sidebar_text.lines.push(Line::from(Span::styled(
                format!("{indent}{marker} {status} {}", app.explorer_label(path)),
                style,
            )));
        }
    } else if app.sidebar_tab() == SidebarTab::Changes {
        sidebar_text.lines.push(Line::from(Span::styled(
            format!("Changes · {}", app.changes_file_view()),
            Style::default().fg(theme.foreground),
        )));
        if app.changes().is_empty() {
            sidebar_text.lines.push(Line::from(Span::styled(
                "Working tree clean",
                Style::default().fg(theme.positive),
            )));
        } else {
            let change_rows = app.changes_tree_rows();
            for row in change_rows.iter().skip(app.changes_tree_scroll()).take(12) {
                let style = if app.changes_tree_row_selected(row) {
                    Style::default().fg(theme.background).bg(theme.accent)
                } else {
                    Style::default().fg(theme.foreground)
                };
                let indent = "  ".repeat(row.depth);
                let (marker, label, status) = if row.kind == ChangeTreeRowKind::File {
                    (" ", row.label.clone(), row.status.clone())
                } else {
                    (
                        if row.expanded { "▾" } else { "▸" },
                        format!("{} ({})", row.label, row.descendant_files),
                        "  ".to_owned(),
                    )
                };
                sidebar_text.lines.push(Line::from(Span::styled(
                    format!("{indent}{marker} {status} {label}"),
                    if row.conflict {
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
                    "v list/tree · Space/s/u stage · c commit · b blame · l log · d diff · h/H hunk · [/ ] hunk {}/{} · o/t ours/theirs · f/p/P sync",
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
    } else if app.sidebar_tab() == SidebarTab::Problems {
        sidebar_text.lines.push(Line::from(Span::styled(
            "Problems · current file",
            Style::default().fg(theme.foreground),
        )));
        let problems = app.problems();
        if problems.is_empty() {
            sidebar_text.lines.push(Line::from(Span::styled(
                "No problems",
                Style::default().fg(theme.positive),
            )));
        } else {
            for (index, problem) in problems.iter().take(12).enumerate() {
                let style = if index == app.problem_index() {
                    Style::default().fg(theme.background).bg(theme.accent)
                } else {
                    Style::default().fg(match problem.severity {
                        1 => theme.error,
                        2 => theme.warning,
                        _ => theme.muted,
                    })
                };
                let marker = match problem.severity {
                    1 => "E",
                    2 => "W",
                    3 => "I",
                    _ => "H",
                };
                sidebar_text.lines.push(Line::from(Span::styled(
                    format!(
                        "{marker} {}:{} {}",
                        problem.line + 1,
                        problem.column + 1,
                        problem.message
                    ),
                    style,
                )));
            }
        }
        sidebar_text.lines.push(Line::from(Span::styled(
            "↑/↓ select · Enter jump",
            Style::default().fg(theme.muted),
        )));
    }
    let sidebar_panel_area = vertical[1];
    if let Some(panel_text) = app.plugin_panel_text_sized(
        PluginPanelLocation::Sidebar,
        sidebar_panel_area.width.saturating_sub(2),
        sidebar_panel_area.height.saturating_sub(2),
    ) {
        app.set_plugin_panel_line_start(
            PluginPanelLocation::Sidebar,
            Some(sidebar_text.lines.len() + 1),
        );
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
            "1 Explore 2 Find\n3 Changes 4 Branches\nCtrl-K D Problems\n5 Terminal 6 Output",
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
    app.set_plugin_panel_line_start(PluginPanelLocation::Bottom, None);
    if app.terminal_fullscreen() {
        draw_terminal_fullscreen(frame, area, app);
        return;
    }
    let theme = active_theme();
    let constraints = if app.bottom_visible() {
        vec![
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(app.bottom_height()),
        ]
    } else {
        vec![Constraint::Length(2), Constraint::Min(3)]
    };
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    app.set_editor_viewport_height(vertical[1].height.saturating_sub(2) as usize);
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
    app.set_editor_viewport_width(content_width.max(1));
    let cursor = app.cursor();
    let wrap_width = app
        .word_wrap()
        .then(|| app.word_wrap_column().min(content_width.max(1)));
    let cursor_display_column =
        display_column(app.file_lines()[cursor.line].as_str(), cursor.column);
    let horizontal_scroll = app
        .horizontal_scroll()
        .min(
            app.file_lines()[cursor.line]
                .chars()
                .map(char_width)
                .sum::<usize>()
                .saturating_sub(content_width),
        )
        .max(cursor_display_column.saturating_sub(content_width.saturating_sub(1)));
    let mut cursor_visual_row = 0usize;
    let mut cursor_visual_column = 0usize;
    let mut visual_row = 0usize;
    let mut editor_lines = Vec::new();
    let mut highlight_mode = HighlightMode::Normal;
    let diff_markers = app.diff_line_markers();
    let conflict_blocks = crate::git::conflict_blocks(app.file_lines());
    let cursor_positions = app.cursor_positions();
    let matching_bracket = app.matching_bracket_position();
    for (index, line) in app.file_lines().iter().enumerate() {
        if app.is_line_hidden(index) {
            let _ = highlight_line_with_state(
                line,
                app.language(),
                theme.foreground,
                theme.background,
                &mut highlight_mode,
            );
            continue;
        }
        let selected = app.is_line_selected(index);
        let diff_marker = diff_markers.get(index).copied().flatten();
        let bookmark_marker = app.bookmark_marker(index);
        let diagnostic = app.diagnostic_severity(index);
        let decoration = app.decoration_style(index);
        let marker = match diagnostic {
            Some(1) => "E",
            Some(2) => "W",
            Some(_) => "I",
            None if decoration.is_some() => match decoration.as_deref() {
                Some("error") => "E",
                Some("warning") => "W",
                Some("info") => "I",
                Some("added") => "+",
                Some("removed") => "-",
                _ => "·",
            },
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
        let mut chunk_start_column = if wrap_width.is_some() {
            0
        } else {
            horizontal_visible_start(line, horizontal_scroll)
        };
        for (chunk_index, chunk) in chunks.into_iter().enumerate() {
            let line_number = if chunk_index == 0 {
                format!("{marker}{:>4} ", index + 1)
            } else {
                "      ".to_owned()
            };
            let background = theme.background;
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
            let mut highlighted = highlight_line_with_state(
                &chunk,
                app.language(),
                foreground,
                background,
                &mut highlight_mode,
            );
            apply_search_highlights(
                &mut highlighted,
                &chunk,
                app.search_query(),
                app.search_regex(),
                app.active_search_match_column(index)
                    .and_then(|column| column.checked_sub(chunk_start_column))
                    .filter(|column| *column < chunk.chars().count()),
            );
            apply_link_highlight(&mut highlighted, &chunk, app.editor_hover_link(index));
            if let Some(matching) = matching_bracket {
                let mut bracket_columns = Vec::new();
                if index == cursor.line {
                    bracket_columns.push(cursor.column);
                }
                if index == matching.line {
                    bracket_columns.push(matching.column);
                }
                let local_columns = bracket_columns
                    .into_iter()
                    .filter_map(|column| column.checked_sub(chunk_start_column))
                    .filter(|column| *column < chunk.chars().count())
                    .collect::<Vec<_>>();
                apply_bracket_highlights(&mut highlighted, &local_columns);
            }
            apply_diagnostic_highlights(&mut highlighted, index, chunk_start_column, app);
            apply_selection_highlights(
                &mut highlighted,
                index,
                chunk_start_column,
                theme.selection,
                selection_modifier,
                app,
            );
            for span in &mut highlighted {
                if decoration.is_some() {
                    span.style = span.style.add_modifier(Modifier::UNDERLINED);
                }
            }
            spans.extend(highlighted);
            editor_lines.push(Line::from(spans));
            visual_row += 1;
            chunk_start_column += chunk.chars().count();
        }
    }
    let plugin_title = app
        .plugin_file_view()
        .map(|(title, _)| format!("Plugin: {title}"));
    let natural_scroll_y =
        cursor_visual_row.saturating_sub(vertical[1].height.saturating_sub(3) as usize);
    let scroll_y = app.editor_scroll_line().unwrap_or(natural_scroll_y);
    let plugin_text = if app.plugin_file_view().is_some() {
        app.plugin_file_view_text_sized(
            vertical[1].width.saturating_sub(2),
            vertical[1].height.saturating_sub(2),
        )
    } else {
        None
    };
    let editor = if let Some(text) = plugin_text {
        Paragraph::new(plugin_view_text(&text, app))
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
                            format!("▶ {}", item.label)
                        } else {
                            format!("  {}", item.label)
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
            LspPopup::Symbols { items, selected } => (
                "Document symbols",
                items
                    .iter()
                    .enumerate()
                    .map(|(index, (name, path, line, column))| {
                        let marker = if index == *selected { "▶" } else { " " };
                        format!(
                            "{marker} {name} · {}:{}:{}",
                            path.display(),
                            line + 1,
                            column + 1
                        )
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
            app.set_terminal_viewport_rows(3);
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
            let output = if app.commit_detail_active() {
                let lines = app.output_reference_lines().unwrap_or_default();
                lines
                    .iter()
                    .skip(app.output_scroll())
                    .take(10)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n")
            } else if !app.blame_lines().is_empty() {
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
                let mut history_lines: Vec<String> = app
                    .history()
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
                    .collect();
                if app.history_has_more()
                    && app.output_scroll() + history_lines.len() >= app.history().len()
                {
                    history_lines.push(if app.history_pending() {
                        "… loading older commits".to_owned()
                    } else {
                        "▶ Load older commits · press ]".to_owned()
                    });
                }
                history_lines.join("\n")
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
            } else if app.pull_request_comments_visible() && !app.pull_request_comments().is_empty()
            {
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
                app.rendered_change_diff()
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
    if app.output_reference_lines().is_some() {
        for (visible_row, line) in bottom.lines.iter_mut().enumerate() {
            let source_row = app.output_scroll().saturating_add(visible_row);
            if let Some(link) = app.output_hover_link(source_row) {
                let text = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>();
                *line = line_with_link(&text, link);
            }
        }
    }
    if app.pull_request_comment_mode() {
        bottom
            .lines
            .push(Line::from(format!("Comment: {}", app.commit_message())));
    }
    if app.diff_view_active() && app.diff_high_contrast() {
        for line in &mut bottom.lines {
            let content = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            let color = if content.contains(" │ ") {
                let Some((left, right)) = content.split_once(" │ ") else {
                    continue;
                };
                match (left.trim().is_empty(), right.trim().is_empty()) {
                    (false, true) => Some(theme.error),
                    (true, false) => Some(theme.positive),
                    _ => None,
                }
            } else if content.starts_with('+') && !content.starts_with("+++") {
                Some(theme.positive)
            } else if content.starts_with('-') && !content.starts_with("---") {
                Some(theme.error)
            } else if content.starts_with('…') {
                Some(theme.warning)
            } else {
                None
            };
            if let Some(color) = color {
                line.style = line.style.fg(color);
            }
        }
    }
    if let Some(panel_text) = app.plugin_panel_text_sized(
        PluginPanelLocation::Bottom,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    ) {
        app.set_plugin_panel_line_start(PluginPanelLocation::Bottom, Some(bottom.lines.len() + 1));
        bottom.lines.push(Line::from(Span::styled(
            "Plugin panel",
            Style::default().fg(theme.positive),
        )));
        bottom
            .lines
            .extend(panel_text.lines().map(|line| Line::from(line.to_owned())));
    }
    let bottom_title = if let Some(title) = app.commit_detail_title() {
        title
    } else if app.bottom_tab() == BottomTab::Terminal {
        let title = app.terminal_title();
        let terminal_name = if title.is_empty() {
            if let Some(directory) = app.terminal_working_directory() {
                let directory = directory.trim_end_matches('/');
                let directory = directory.rsplit('/').next().unwrap_or(directory);
                format!(
                    "Terminal · {directory} ({}/{})",
                    app.active_terminal() + 1,
                    app.terminal_count()
                )
            } else {
                format!(
                    "Terminal {}/{}",
                    app.active_terminal() + 1,
                    app.terminal_count()
                )
            }
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
    } else if app.pull_request_comments_visible() && !app.pull_request_comments().is_empty() {
        "PR comments · m diff · Enter open comment".to_owned()
    } else if app.pull_request_comments_visible() {
        "Loading PR comments… · m diff".to_owned()
    } else if !app.change_diff().is_empty() {
        format!(
            "Output · {} {} · {} · Enter/e expand · m comments",
            if app.pull_request_review_active() {
                "PR diff"
            } else {
                "Git diff"
            },
            if app.diff_split() { "split" } else { "unified" },
            if app.pull_request_review_active() {
                "c comment"
            } else if app.diff_full_context() {
                "c changes"
            } else {
                "c full"
            }
        )
    } else {
        app.bottom_tab().label().to_owned()
    };
    let paragraph = Paragraph::new(bottom)
        .style(Style::default().fg(theme.muted))
        .block(panel(
            &bottom_title,
            app.focus() == Focus::Terminal || app.focus() == Focus::Output,
        ));
    if app.diff_view_active() && app.diff_word_wrap() {
        frame.render_widget(paragraph.wrap(Wrap { trim: false }), vertical[2]);
    } else {
        frame.render_widget(paragraph, vertical[2]);
    }
}

fn draw_terminal_fullscreen(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = active_theme();
    app.set_terminal_viewport_rows(area.height.saturating_sub(2) as usize);
    let mut text = Text::default();
    for row in app.terminal_screen_styled_lines() {
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
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(theme.foreground).bg(theme.background))
            .block(panel("Terminal fullscreen · Alt-T restore", true)),
        area,
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

fn horizontal_visible_start(line: &str, horizontal_scroll: usize) -> usize {
    let mut display = 0usize;
    for (index, character) in line.chars().enumerate() {
        let width = char_width(character);
        if display.saturating_add(width) > horizontal_scroll {
            return index;
        }
        display = display.saturating_add(width);
    }
    line.chars().count()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HighlightMode {
    Normal,
    BlockComment,
    String(char),
    MarkdownFence(char, usize),
}

#[cfg(test)]
fn highlight_line(
    text: &str,
    language: &str,
    foreground: Color,
    background: Color,
) -> Vec<Span<'static>> {
    let mut mode = HighlightMode::Normal;
    highlight_line_with_state(text, language, foreground, background, &mut mode)
}

fn highlight_line_with_state(
    text: &str,
    language: &str,
    foreground: Color,
    background: Color,
    mode: &mut HighlightMode,
) -> Vec<Span<'static>> {
    highlight_line_with_theme_state(text, language, foreground, background, mode, active_theme())
}

fn highlight_line_with_theme_state(
    text: &str,
    language: &str,
    foreground: Color,
    background: Color,
    mode: &mut HighlightMode,
    theme: Theme,
) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let mut index = 0;
    if matches!(language, "c" | "cpp" | "h" | "cc" | "hpp" | "hxx" | "cxx") {
        let first = chars
            .iter()
            .position(|character| !character.is_whitespace())
            .unwrap_or(chars.len());
        if chars.get(first) == Some(&'#') {
            return vec![Span::styled(
                text.to_owned(),
                Style::default()
                    .fg(theme.syntax.heading)
                    .bg(background)
                    .add_modifier(Modifier::BOLD),
            )];
        }
    }
    if let HighlightMode::MarkdownFence(marker, length) = *mode {
        let first = chars
            .iter()
            .position(|character| !character.is_whitespace())
            .unwrap_or(chars.len());
        let close = first + length <= chars.len()
            && chars[first..]
                .iter()
                .take(length)
                .all(|character| *character == marker);
        if close {
            *mode = HighlightMode::Normal;
            spans.push(Span::styled(
                text.to_owned(),
                Style::default()
                    .fg(theme.syntax.code)
                    .bg(background)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                text.to_owned(),
                Style::default().fg(foreground).bg(background),
            ));
        }
        return spans;
    }
    if matches!(language, "md" | "markdown") {
        let first = chars
            .iter()
            .position(|character| !character.is_whitespace())
            .unwrap_or(chars.len());
        if let Some(marker) = chars
            .get(first)
            .copied()
            .filter(|marker| *marker == '`' || *marker == '~')
        {
            let length = chars[first..]
                .iter()
                .take_while(|character| **character == marker)
                .count();
            if length >= 3 {
                *mode = HighlightMode::MarkdownFence(marker, length);
                spans.push(Span::styled(
                    text.to_owned(),
                    Style::default()
                        .fg(theme.syntax.code)
                        .bg(background)
                        .add_modifier(Modifier::BOLD),
                ));
                return spans;
            }
        }
    }
    if *mode == HighlightMode::Normal && matches!(language, "md" | "markdown") {
        let first = chars
            .iter()
            .position(|character| !character.is_whitespace())
            .unwrap_or(chars.len());
        let marker = chars.get(first).copied();
        let marker_end = if marker == Some('#') {
            let mut end = first;
            while chars.get(end) == Some(&'#') {
                end += 1;
            }
            (chars.get(end).is_none() || chars[end].is_whitespace()).then_some(end)
        } else if matches!(marker, Some('-' | '*' | '>'))
            && chars
                .get(first + 1)
                .is_some_and(|character| character.is_whitespace())
        {
            Some(first + 1)
        } else {
            None
        };
        if let Some(end) = marker_end {
            if first > 0 {
                spans.push(Span::styled(
                    chars[..first].iter().collect::<String>(),
                    Style::default().fg(foreground).bg(background),
                ));
            }
            spans.push(Span::styled(
                chars[first..end].iter().collect::<String>(),
                Style::default()
                    .fg(theme.syntax.heading)
                    .bg(background)
                    .add_modifier(Modifier::BOLD),
            ));
            index = end;
        }
    }
    while index < chars.len() {
        if *mode != HighlightMode::Normal {
            let start = index;
            let terminator = match *mode {
                HighlightMode::BlockComment => (chars[index..]
                    .windows(2)
                    .position(|pair| pair == ['*', '/']))
                .map(|offset| index + offset + 2),
                HighlightMode::String(quote) => chars[index..]
                    .iter()
                    .enumerate()
                    .find(|(offset, character)| {
                        **character == quote
                            && (index + *offset == 0 || chars[index + *offset - 1] != '\\')
                    })
                    .map(|(offset, _)| index + offset + 1),
                HighlightMode::MarkdownFence(_, _) => None,
                HighlightMode::Normal => None,
            };
            index = terminator.unwrap_or(chars.len());
            spans.push(Span::styled(
                chars[start..index].iter().collect::<String>(),
                Style::default().fg(theme.syntax.comment).bg(background),
            ));
            if terminator.is_none() {
                break;
            }
            *mode = HighlightMode::Normal;
            continue;
        }
        if let Some(prefix) = comment_prefix(language) {
            let marker: Vec<char> = prefix.chars().collect();
            if chars[index..].starts_with(&marker) {
                spans.push(Span::styled(
                    chars[index..].iter().collect::<String>(),
                    Style::default().fg(theme.syntax.comment).bg(background),
                ));
                break;
            }
        }
        let character = chars[index];
        if character == '/' && chars.get(index + 1) == Some(&'*') {
            let start = index;
            index += 2;
            while index + 1 < chars.len() && !(chars[index] == '*' && chars[index + 1] == '/') {
                index += 1;
            }
            if index + 1 < chars.len() {
                index += 2;
            } else {
                *mode = HighlightMode::BlockComment;
            }
            spans.push(Span::styled(
                chars[start..index].iter().collect::<String>(),
                Style::default().fg(theme.syntax.comment).bg(background),
            ));
        } else if matches!(character, '"' | '\'' | '`') {
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
            if index >= chars.len() && chars.last() != Some(&quote) {
                *mode = HighlightMode::String(quote);
            }
            let mut after = index;
            while chars
                .get(after)
                .is_some_and(|character| character.is_whitespace())
            {
                after += 1;
            }
            let is_key = matches!(language, "json" | "jsonc" | "json5" | "toml")
                && chars.get(after) == Some(&':');
            spans.push(Span::styled(
                chars[start..index].iter().collect::<String>(),
                Style::default()
                    .fg(if is_key {
                        theme.syntax.property
                    } else {
                        theme.syntax.string
                    })
                    .bg(background),
            ));
        } else if character.is_ascii_digit() {
            let start = index;
            index += 1;
            while index < chars.len() && (chars[index].is_ascii_digit() || chars[index] == '.') {
                index += 1;
            }
            spans.push(Span::styled(
                chars[start..index].iter().collect::<String>(),
                Style::default().fg(theme.syntax.number).bg(background),
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
            let color = if is_literal(language, &word) {
                theme.syntax.constant
            } else if is_builtin(language, &word) {
                theme.syntax.builtin
            } else if is_keyword(language, &word) {
                theme.syntax.keyword
            } else if chars[index..]
                .iter()
                .find(|character| !character.is_whitespace())
                == Some(&'(')
            {
                theme.syntax.function
            } else if word.chars().next().is_some_and(char::is_uppercase) {
                theme.syntax.type_name
            } else {
                theme.syntax.variable
            };
            spans.push(Span::styled(
                word,
                Style::default().fg(color).bg(background),
            ));
        } else {
            let color = if matches!(
                character,
                '+' | '-' | '*' | '/' | '=' | '!' | '<' | '>' | '|' | '&' | '%' | '^' | '~'
            ) {
                theme.syntax.operator
            } else if character.is_ascii_punctuation() {
                theme.syntax.punctuation
            } else {
                foreground
            };
            spans.push(Span::styled(
                character.to_string(),
                Style::default().fg(color).bg(background),
            ));
            index += 1;
        }
    }
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), Style::default().bg(background)));
    }
    spans
}

fn apply_search_highlights(
    spans: &mut [Span<'static>],
    text: &str,
    query: &str,
    use_regex: bool,
    active_start_column: Option<usize>,
) {
    apply_search_highlights_with_theme(
        spans,
        text,
        query,
        use_regex,
        active_start_column,
        active_theme(),
    );
}

fn apply_search_highlights_with_theme(
    spans: &mut [Span<'static>],
    text: &str,
    query: &str,
    use_regex: bool,
    active_start_column: Option<usize>,
    theme: Theme,
) {
    if query.is_empty() || text.is_empty() {
        return;
    }
    let ranges = if use_regex {
        Regex::new(query)
            .ok()
            .map(|regex| regex.find_iter(text).map(|m| m.range()).collect::<Vec<_>>())
            .unwrap_or_default()
    } else if query.chars().any(char::is_uppercase) {
        text.match_indices(query)
            .map(|(start, value)| start..start + value.len())
            .collect::<Vec<_>>()
    } else {
        let mut ranges = Vec::new();
        let mut offset = 0;
        while offset < text.len() {
            let remaining = &text[offset..];
            if remaining
                .get(..query.len())
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(query))
            {
                ranges.push(offset..offset + query.len());
                offset += query.len();
            } else if let Some(character) = remaining.chars().next() {
                offset += character.len_utf8();
            } else {
                break;
            }
        }
        ranges
    };
    if ranges.is_empty() {
        return;
    }
    let active_start_byte = active_start_column
        .and_then(|column| text.char_indices().nth(column).map(|(byte, _)| byte));
    let mut byte_offset = 0;
    for span in spans {
        let span_text = span.content.as_ref();
        let span_start = byte_offset;
        byte_offset += span_text.len();
        let span_end = byte_offset;
        let matching_ranges = ranges
            .iter()
            .filter(|range| range.start < span_end && range.end > span_start)
            .collect::<Vec<_>>();
        if !matching_ranges.is_empty() {
            let active = active_start_byte.is_some_and(|active_start| {
                matching_ranges
                    .iter()
                    .any(|range| range.start == active_start)
            });
            span.style = span
                .style
                .fg(theme.background)
                .bg(if active {
                    theme.search_active
                } else {
                    theme.search_match
                })
                .add_modifier(Modifier::BOLD);
        }
    }
}

fn apply_link_highlight(spans: &mut [Span<'static>], text: &str, link: Option<&str>) {
    let Some(link) = link else {
        return;
    };
    let Some(start) = text.find(link) else {
        return;
    };
    let end = start + link.len();
    let mut offset = 0;
    for span in spans {
        let span_text = span.content.as_ref();
        let span_start = offset;
        let span_end = span_start + span_text.len();
        if span_start < end && span_end > start {
            span.style = span
                .style
                .fg(active_theme().syntax.link)
                .add_modifier(Modifier::UNDERLINED);
        }
        offset = span_end;
    }
}

fn plugin_view_text(text: &str, app: &App) -> Text<'static> {
    Text::from_iter(text.lines().enumerate().map(|(index, line)| {
        let Some(link) = app.plugin_file_view_hover_link(index) else {
            return Line::from(line.to_owned());
        };
        line_with_link(line, link)
    }))
}

fn line_with_link(line: &str, link: &str) -> Line<'static> {
    let Some(start) = line.find(link) else {
        return Line::from(line.to_owned());
    };
    let end = start + link.len();
    Line::from(vec![
        Span::raw(line[..start].to_owned()),
        Span::styled(
            link.to_owned(),
            Style::default()
                .fg(active_theme().accent)
                .add_modifier(Modifier::UNDERLINED),
        ),
        Span::raw(line[end..].to_owned()),
    ])
}

fn apply_bracket_highlights(spans: &mut Vec<Span<'static>>, columns: &[usize]) {
    if columns.is_empty() {
        return;
    }
    let columns = columns
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    let mut offset = 0;
    let mut rebuilt = Vec::with_capacity(spans.len() + columns.len());
    for span in spans.drain(..) {
        let content = span.content.to_string();
        let char_count = content.chars().count();
        if !columns
            .iter()
            .any(|column| (*column >= offset) && (*column < offset + char_count))
        {
            offset += char_count;
            rebuilt.push(span);
            continue;
        }
        for (index, character) in content.chars().enumerate() {
            let mut style = span.style;
            if columns.contains(&(offset + index)) {
                style = style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
            }
            rebuilt.push(Span::styled(character.to_string(), style));
        }
        offset += char_count;
    }
    *spans = rebuilt;
}

fn apply_selection_highlights(
    spans: &mut Vec<Span<'static>>,
    line: usize,
    start_column: usize,
    selection_background: Color,
    selection_modifier: Modifier,
    app: &App,
) {
    if !app.is_line_selected(line) {
        return;
    }
    let original = std::mem::take(spans);
    let mut result = Vec::new();
    let mut column = start_column;
    for span in original {
        let style = span.style;
        for character in span.content.chars() {
            let mut character_style = style;
            if app.is_position_selected(line, column) {
                character_style = character_style
                    .bg(selection_background)
                    .add_modifier(selection_modifier);
            }
            result.push(Span::styled(character.to_string(), character_style));
            column += 1;
        }
    }
    *spans = result;
}

fn apply_diagnostic_highlights(
    spans: &mut Vec<Span<'static>>,
    line: usize,
    start_column: usize,
    app: &App,
) {
    if app.diagnostic_severity(line).is_none() {
        return;
    }
    let original = std::mem::take(spans);
    let mut result = Vec::new();
    let mut column = start_column;
    for span in original {
        let style = span.style;
        for character in span.content.chars() {
            let character_style = if app.is_diagnostic_position(line, column) {
                style.add_modifier(Modifier::UNDERLINED)
            } else {
                style
            };
            result.push(Span::styled(character.to_string(), character_style));
            column += 1;
        }
    }
    *spans = result;
}

fn comment_prefix(language: &str) -> Option<&'static str> {
    match language {
        "py" | "pyi" | "sh" | "bash" | "zsh" | "yaml" | "yml" | "toml" => Some("#"),
        "lua" => Some("--"),
        "rs" | "go" | "js" | "jsx" | "ts" | "tsx" | "java" | "c" | "cpp" | "h" | "cc" | "hpp"
        | "hxx" | "cxx" => Some("//"),
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
        "c" | "cpp" | "h" | "cc" | "hpp" | "hxx" | "cxx" => matches!(
            word,
            "auto"
                | "break"
                | "case"
                | "char"
                | "class"
                | "const"
                | "constexpr"
                | "continue"
                | "default"
                | "delete"
                | "do"
                | "double"
                | "else"
                | "enum"
                | "extern"
                | "float"
                | "for"
                | "friend"
                | "goto"
                | "inline"
                | "int"
                | "long"
                | "namespace"
                | "new"
                | "nullptr"
                | "private"
                | "protected"
                | "public"
                | "register"
                | "restrict"
                | "short"
                | "signed"
                | "sizeof"
                | "static"
                | "struct"
                | "switch"
                | "template"
                | "this"
                | "typedef"
                | "typename"
                | "union"
                | "unsigned"
                | "using"
                | "virtual"
                | "void"
                | "volatile"
                | "while"
        ),
        _ => false,
    };
    common || language_keyword
}

fn is_literal(language: &str, word: &str) -> bool {
    if matches!(word, "true" | "false" | "null" | "nil") {
        return true;
    }
    match language {
        "rs" => matches!(word, "Some" | "None" | "Ok" | "Err"),
        "py" | "pyi" => matches!(word, "True" | "False" | "None"),
        "toml" | "yaml" | "yml" => matches!(word, "true" | "false" | "null"),
        _ => false,
    }
}

fn is_builtin(language: &str, word: &str) -> bool {
    match language {
        "rs" | "rust" => matches!(word, "Some" | "None" | "Ok" | "Err" | "String" | "Vec"),
        "py" | "pyw" | "python" => matches!(
            word,
            "print"
                | "len"
                | "range"
                | "str"
                | "int"
                | "bool"
                | "list"
                | "dict"
                | "set"
                | "tuple"
                | "open"
                | "enumerate"
                | "zip"
                | "map"
                | "filter"
        ),
        "go" => matches!(
            word,
            "append"
                | "cap"
                | "close"
                | "copy"
                | "delete"
                | "len"
                | "make"
                | "new"
                | "panic"
                | "recover"
        ),
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "typescript" => matches!(
            word,
            "Array"
                | "Boolean"
                | "Date"
                | "Error"
                | "JSON"
                | "Map"
                | "Math"
                | "Number"
                | "Object"
                | "Promise"
                | "Set"
                | "String"
                | "Symbol"
        ),
        "lua" => matches!(
            word,
            "assert"
                | "error"
                | "ipairs"
                | "pairs"
                | "pcall"
                | "print"
                | "require"
                | "select"
                | "tonumber"
                | "tostring"
                | "type"
                | "xpcall"
        ),
        _ => false,
    }
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
    let terminal_colors = active_theme().terminal_colors;
    match color {
        AnsiColor::Basic(index) => terminal_colors[usize::from(index.min(7))],
        AnsiColor::Bright(index) => terminal_colors[usize::from(index.min(7)) + 8],
        AnsiColor::Indexed(index) if index < 16 => terminal_colors[usize::from(index)],
        AnsiColor::Indexed(index) => Color::Indexed(index),
        AnsiColor::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
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
    let height = area.height.clamp(1, 24);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 3,
        width,
        height,
    );
    let rows = app.settings_rows();
    let visible_rows = popup.height.saturating_sub(3) as usize;
    let start = app
        .settings_index()
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(rows.len().saturating_sub(visible_rows));
    let end = (start + visible_rows).min(rows.len());
    let mut lines = vec!["↑/↓ select · Enter/Space toggle · Ctrl-S save · Esc close".to_owned()];
    lines.extend(rows.iter().enumerate().skip(start).take(end - start).map(
        |(index, (label, enabled))| {
            let marker = if index == app.settings_index() {
                "▶"
            } else {
                " "
            };
            let value = if *enabled { "on" } else { "off" };
            format!("{marker} {label:<24} {value}")
        },
    ));
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
    let results = app.quick_open_results();
    let command_mode = app.quick_open_title() == "Command Palette";
    let help_mode = app.quick_open_title() == "Keyboard Help";
    let commands = if command_mode {
        app.command_palette_items_for_quick_open()
    } else {
        Vec::new()
    };
    let help_items = if help_mode {
        app.quick_open_help_items()
    } else {
        Vec::new()
    };
    let row_count = if command_mode {
        commands.len()
    } else if help_mode {
        help_items.len()
    } else {
        results.len()
    };
    let height = if matches!(
        app.quick_open_title(),
        "Save As" | "Save Workspace" | "Add Workspace Folder"
    ) {
        3
    } else {
        (3 + row_count.min(8) as u16).min(area.height.saturating_sub(2))
    };
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let mut lines = vec![format!("Path: {}", app.quick_open_query())];
    if command_mode {
        lines.extend(
            commands
                .iter()
                .take(8)
                .enumerate()
                .map(|(index, (id, title))| {
                    let marker = if index == app.quick_open_index() {
                        "▶"
                    } else {
                        " "
                    };
                    format!("{marker} {title} · {id}")
                }),
        );
    } else if help_mode {
        lines.extend(
            help_items
                .iter()
                .take(8)
                .enumerate()
                .map(|(index, (key, description))| {
                    let marker = if index == app.quick_open_index() {
                        "▶"
                    } else {
                        " "
                    };
                    format!("{marker} {key:<14} {description}")
                }),
        );
    } else if app.quick_open_title() != "Save As" {
        lines.extend(results.iter().take(8).enumerate().map(|(index, path)| {
            let label = app
                .workspace_roots()
                .iter()
                .find_map(|root| path.strip_prefix(root).ok())
                .map_or_else(
                    || path.display().to_string(),
                    |relative| relative.display().to_string(),
                );
            let marker = if index == app.quick_open_index() {
                "▶"
            } else {
                " "
            };
            format!("{marker} {label}")
        }));
    }
    frame.render_widget(
        Paragraph::new(lines.join("\n"))
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
    if let Some(echo) = app.plugin_echo() {
        app.set_plugin_status_hits(Vec::new());
        frame.render_widget(
            Paragraph::new(format!(" {echo}"))
                .style(Style::default().fg(theme.background).bg(theme.accent)),
            area,
        );
        return;
    }
    let status_message = app
        .plugin_notification()
        .map(|(message, level)| format!("[{level}] {message}"))
        .unwrap_or_else(|| app.status_message().to_owned());
    let cursor = app.cursor();
    let diagnostic = app
        .diagnostic_message(cursor.line)
        .map(|message| format!(" · {message}"))
        .unwrap_or_default();
    let blame = app
        .blame_lines()
        .get(cursor.line)
        .map(|line| {
            format!(
                " · {} {} {}",
                &line.commit[..line.commit.len().min(8)],
                line.author,
                line.line
            )
        })
        .unwrap_or_default();
    let (plugin_left_items, plugin_right_items) = app.plugin_status_items_detailed();
    let plugin_left = plugin_left_items
        .iter()
        .map(|(_, _, text)| text.as_str())
        .collect::<Vec<_>>()
        .join(" · ");
    let plugin_right = plugin_right_items
        .iter()
        .map(|(_, _, text)| text.as_str())
        .collect::<Vec<_>>()
        .join(" · ");
    let plugin_left_suffix = if plugin_left.is_empty() {
        String::new()
    } else {
        format!(" · {plugin_left}")
    };
    let plugin_right_suffix = if plugin_right.is_empty() {
        String::new()
    } else {
        format!(" · {plugin_right}")
    };
    let text = format!(
        " {}{}{}{}   {}   Ln {}, Col {}   UTF-8   {}   {}   {}   {}{}   Tab: focus · Ctrl+Q: quit",
        status_message,
        diagnostic,
        blame,
        plugin_left_suffix,
        app.file_path_label(),
        cursor.line + 1,
        cursor.column + 1,
        app.language(),
        app.focus().label(),
        app.branch().map_or("Git:—", |branch| branch),
        app.indent_summary(),
        plugin_right_suffix,
    );
    let width = |value: &str| -> u16 {
        value
            .chars()
            .map(|character| character.width().unwrap_or(1) as u16)
            .fold(0, u16::saturating_add)
    };
    let mut hits = Vec::new();
    let left_prefix = format!(" {}{}{}", status_message, diagnostic, blame);
    let mut position = width(&left_prefix);
    for (index, (runtime_index, id, item)) in plugin_left_items.iter().enumerate() {
        position = position.saturating_add(width(" · "));
        let start = position;
        position = position.saturating_add(width(item));
        hits.push((start, position, *runtime_index, id.clone()));
        if index + 1 < plugin_left_items.len() {
            position = position.saturating_add(width(" · "));
        }
    }
    if !plugin_right_items.is_empty() {
        let right_start = width(&text).saturating_sub(width(&plugin_right_suffix));
        let mut position = right_start.saturating_add(width(" · "));
        for (index, (runtime_index, id, item)) in plugin_right_items.iter().enumerate() {
            let start = position;
            position = position.saturating_add(width(item));
            hits.push((start, position, *runtime_index, id.clone()));
            if index + 1 < plugin_right_items.len() {
                position = position.saturating_add(width(" · "));
            }
        }
    }
    app.set_plugin_status_hits(hits);
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(theme.background).bg(theme.accent)),
        area,
    );
}

fn draw_small(frame: &mut Frame<'_>, app: &App, area: Rect) {
    app.set_plugin_status_hits(Vec::new());
    let theme = active_theme();
    frame.render_widget(Clear, area);
    let cursor = app.cursor();
    let content = format!(
        "mtide · {}\n\nLine {}, column {}\n\n{}\n\nCtrl+Q quit · Tab focus · Ctrl+B sidebar",
        app.file_name(),
        cursor.line + 1,
        cursor.column + 1,
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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use ratatui::{backend::TestBackend, Terminal};

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
    fn search_highlights_follow_smart_case_and_regex() {
        let theme = active_theme();
        let mut smart_case =
            highlight_line("Needle needle", "txt", theme.foreground, theme.background);
        apply_search_highlights(&mut smart_case, "Needle needle", "Needle", false, None);
        assert_eq!(
            smart_case
                .iter()
                .filter(|span| span.style.bg == Some(theme.warning))
                .count(),
            1
        );

        let mut regex = highlight_line("one 42 two", "txt", theme.foreground, theme.background);
        apply_search_highlights(&mut regex, "one 42 two", r"\d+", true, None);
        assert!(regex
            .iter()
            .any(|span| span.style.bg == Some(theme.warning)));
    }

    #[test]
    fn selected_search_match_uses_ttt_active_match_color() {
        let theme = Theme::from_json(&serde_json::json!({
            "editor": {
                "searchMatch": {"bg": "#112233"},
                "searchActive": {"bg": "#aabbcc"}
            }
        }));
        let mut spans = vec![
            Span::styled("needle".to_owned(), Style::default()),
            Span::raw(" ".to_owned()),
            Span::styled("needle".to_owned(), Style::default()),
        ];
        apply_search_highlights_with_theme(
            &mut spans,
            "needle needle",
            "needle",
            false,
            Some(7),
            theme,
        );
        assert_eq!(
            spans[0].style.bg,
            Some(ratatui::style::Color::Rgb(17, 34, 51))
        );
        assert_eq!(
            spans[2].style.bg,
            Some(ratatui::style::Color::Rgb(170, 187, 204))
        );
    }

    #[test]
    fn ttt_syntax_theme_colors_drive_lexer_token_roles() {
        let theme = Theme::from_json(&serde_json::json!({
            "default": {"fg": "#ffffff", "bg": "#000000"},
            "syntax": {
                "keyword": {"fg": "#010203"},
                "function": {"fg": "#040506"},
                "number": {"fg": "#070809"},
                "comment": {"fg": "#0a0b0c"},
                "builtin": {"fg": "#0d0e0f"}
            }
        }));
        let spans = highlight_line_with_theme_state(
            "fn run() { 42 } // note",
            "rs",
            theme.foreground,
            theme.background,
            &mut HighlightMode::Normal,
            theme,
        );
        let color_of = |text: &str| {
            spans
                .iter()
                .find(|span| span.content.as_ref() == text)
                .and_then(|span| span.style.fg)
        };
        assert_eq!(color_of("fn"), Some(ratatui::style::Color::Rgb(1, 2, 3)));
        assert_eq!(color_of("run"), Some(ratatui::style::Color::Rgb(4, 5, 6)));
        assert_eq!(color_of("42"), Some(ratatui::style::Color::Rgb(7, 8, 9)));
        assert_eq!(
            color_of("// note"),
            Some(ratatui::style::Color::Rgb(10, 11, 12))
        );
        let python = highlight_line_with_theme_state(
            "len(values)",
            "py",
            theme.foreground,
            theme.background,
            &mut HighlightMode::Normal,
            theme,
        );
        assert_eq!(
            python
                .iter()
                .find(|span| span.content.as_ref() == "len")
                .and_then(|span| span.style.fg),
            Some(ratatui::style::Color::Rgb(13, 14, 15))
        );
    }

    #[test]
    fn structured_languages_receive_contextual_highlighting() {
        let theme = active_theme();
        let markdown = highlight_line("  ## Title", "markdown", theme.foreground, theme.background);
        assert_eq!(markdown[1].content, "##");
        assert_eq!(markdown[1].style.fg, Some(theme.accent));
        assert_eq!(
            markdown[1].style.add_modifier(Modifier::BOLD),
            markdown[1].style
        );

        let json = highlight_line(
            r#"{"name": true}"#,
            "json",
            theme.foreground,
            theme.background,
        );
        assert!(json.iter().any(|span| {
            span.content.as_ref() == r#""name""# && span.style.fg == Some(theme.accent)
        }));

        let block_comment =
            highlight_line("/* note */ value", "rs", theme.foreground, theme.background);
        assert_eq!(block_comment[0].style.fg, Some(theme.muted));
    }

    #[test]
    fn c_and_cpp_headers_receive_directive_and_keyword_highlighting() {
        let theme = active_theme();
        let directive =
            highlight_line("#include <vector>", "h", theme.foreground, theme.background);
        assert_eq!(directive[0].style.fg, Some(theme.accent));
        let declaration =
            highlight_line("static int main()", "h", theme.foreground, theme.background);
        let declaration_keywords: Vec<_> = declaration
            .iter()
            .filter(|span| matches!(span.content.as_ref(), "static" | "int"))
            .collect();
        assert_eq!(declaration_keywords.len(), 2);
        assert!(declaration_keywords
            .iter()
            .all(|span| span.style.fg == Some(theme.accent)));
        let template = highlight_line(
            "template <typename T>",
            "hpp",
            theme.foreground,
            theme.background,
        );
        let template_keywords: Vec<_> = template
            .iter()
            .filter(|span| matches!(span.content.as_ref(), "template" | "typename"))
            .collect();
        assert_eq!(template_keywords.len(), 2);
        assert!(template_keywords
            .iter()
            .all(|span| span.style.fg == Some(theme.accent)));
    }

    #[test]
    fn literals_receive_distinct_warning_highlighting() {
        let theme = active_theme();
        let rust = highlight_line(
            "let value = Some(true);",
            "rs",
            theme.foreground,
            theme.background,
        );
        assert!(rust.iter().any(|span| {
            span.content.as_ref() == "Some" && span.style.fg == Some(theme.warning)
        }));
        assert!(rust.iter().any(|span| {
            span.content.as_ref() == "true" && span.style.fg == Some(theme.warning)
        }));
        let python = highlight_line("value = None", "py", theme.foreground, theme.background);
        assert!(python.iter().any(|span| {
            span.content.as_ref() == "None" && span.style.fg == Some(theme.warning)
        }));
    }

    #[test]
    fn multiline_highlighting_preserves_comment_and_string_state() {
        let theme = active_theme();
        let mut mode = HighlightMode::Normal;
        let first = highlight_line_with_state(
            "/* begin",
            "rs",
            theme.foreground,
            theme.background,
            &mut mode,
        );
        assert_eq!(mode, HighlightMode::BlockComment);
        assert_eq!(first[0].style.fg, Some(theme.muted));
        let second = highlight_line_with_state(
            "end */ value",
            "rs",
            theme.foreground,
            theme.background,
            &mut mode,
        );
        assert_eq!(mode, HighlightMode::Normal);
        assert!(second.iter().any(|span| span.content.as_ref() == "value"));

        let mut mode = HighlightMode::Normal;
        let _ = highlight_line_with_state(
            "let value = \"first",
            "rs",
            theme.foreground,
            theme.background,
            &mut mode,
        );
        assert_eq!(mode, HighlightMode::String('"'));
        let _ = highlight_line_with_state(
            "second\";",
            "rs",
            theme.foreground,
            theme.background,
            &mut mode,
        );
        assert_eq!(mode, HighlightMode::Normal);
    }

    #[test]
    fn markdown_fenced_code_preserves_code_block_state() {
        let theme = active_theme();
        let mut mode = HighlightMode::Normal;
        let opening = highlight_line_with_state(
            "```rust",
            "markdown",
            theme.foreground,
            theme.background,
            &mut mode,
        );
        assert_eq!(mode, HighlightMode::MarkdownFence('`', 3));
        assert_eq!(opening[0].style.fg, Some(theme.accent));
        let content = highlight_line_with_state(
            "# not a markdown heading",
            "markdown",
            theme.foreground,
            theme.background,
            &mut mode,
        );
        assert_eq!(mode, HighlightMode::MarkdownFence('`', 3));
        assert_eq!(content[0].style.fg, Some(theme.foreground));
        let closing = highlight_line_with_state(
            "```",
            "markdown",
            theme.foreground,
            theme.background,
            &mut mode,
        );
        assert_eq!(mode, HighlightMode::Normal);
        assert_eq!(closing[0].style.fg, Some(theme.accent));
    }

    #[test]
    fn ctrl_hover_link_highlight_marks_only_the_visible_link() {
        let mut spans = vec![
            Span::raw("see "),
            Span::raw("https://example.com"),
            Span::raw(" now"),
        ];
        apply_link_highlight(
            &mut spans,
            "see https://example.com now",
            Some("https://example.com"),
        );
        assert_eq!(spans[0].style, Style::default());
        assert_ne!(spans[1].style, Style::default());
        assert_eq!(spans[2].style, Style::default());
    }

    #[test]
    fn alt_mouse_wheel_scrolls_editor_horizontally() {
        let mut app = App::new(&[]).expect("app");
        app.set_focus(Focus::Editor);
        let area = Rect::new(0, 0, 120, 40);
        let editor = editor_content_area(&app, area).expect("editor area");
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: editor.x.saturating_add(8),
                row: editor.y.saturating_add(1),
                modifiers: KeyModifiers::ALT,
            },
            area,
        );
        assert_eq!(app.horizontal_scroll(), 4);
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: editor.x.saturating_add(8),
                row: editor.y.saturating_add(1),
                modifiers: KeyModifiers::ALT,
            },
            area,
        );
        assert_eq!(app.horizontal_scroll(), 0);
    }

    #[test]
    fn search_sidebar_mouse_selects_fields_and_replace_cursor() {
        let mut app = App::new(&[]).expect("app");
        app.select_sidebar_tab_by_index(1);
        app.set_replace_focus(0);
        let area = Rect::new(0, 0, 120, 40);
        let sidebar = sidebar_area(&app, area).expect("sidebar area");
        let body = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(3),
                Constraint::Length(3),
            ])
            .split(sidebar)[1];
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: body.x + 15,
                row: body.y + 2,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert_eq!(app.search_field(), 1);
        assert_eq!(app.search_input_cursor(), 0);

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: body.x + 12,
                row: body.y + 5,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert!(app.replace_mode());
        assert_eq!(app.search_field(), 0);
        assert_eq!(app.focus(), Focus::Search);
    }

    #[test]
    fn changes_sidebar_renders_hierarchy_and_mouse_collapses_directories() {
        let root =
            std::env::temp_dir().join(format!("mtide-ui-changes-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).expect("source directory");
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .expect("git init")
            .success());
        std::fs::write(root.join("src/a.rs"), "a\n").expect("file a");
        std::fs::write(root.join("src/b.rs"), "b\n").expect("file b");
        let config = root.with_extension("settings.json");
        std::fs::write(&config, r#"{"git":{"fileView":"tree"}}"#).expect("settings");
        let args = vec![
            "--config".to_owned(),
            config.to_string_lossy().into_owned(),
            root.to_string_lossy().into_owned(),
        ];
        let mut app = App::new(&args).expect("app");
        app.select_sidebar_tab_by_index(2);
        let area = Rect::new(0, 0, 120, 40);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, &app)).expect("draw tree");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("src (2)"));
        assert!(rendered.contains("Changes (2)"));
        assert!(rendered.contains("a.rs"));

        let sidebar = sidebar_area(&app, area).expect("sidebar area");
        let body = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(3),
                Constraint::Length(3),
            ])
            .split(sidebar)[1];
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: body.x + 4,
                row: body.y + 3,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert_eq!(app.changes_tree_rows().len(), 2);
        terminal
            .draw(|frame| draw(frame, &app))
            .expect("draw collapsed tree");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("▸"));
        assert!(!rendered.contains("a.rs"));
        let _ = std::fs::remove_file(config);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn dragging_sidebar_divider_resizes_and_releases() {
        let mut app = App::new(&[]).expect("app");
        let area = Rect::new(0, 0, 120, 40);
        let sidebar = sidebar_area(&app, area).expect("sidebar area");
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: sidebar.right(),
                row: sidebar.y + 4,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert!(app.sidebar_resizing());
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 36,
                row: sidebar.y + 4,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert_eq!(app.sidebar_width(), 36);
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 36,
                row: sidebar.y + 4,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert!(!app.sidebar_resizing());
    }

    #[test]
    fn dragging_bottom_divider_resizes_and_releases() {
        let mut app = App::new(&[]).expect("app");
        let area = Rect::new(0, 0, 120, 40);
        let bottom = terminal_area(&app, area).expect("bottom area");
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: bottom.x + 4,
                row: bottom.y,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert!(app.bottom_resizing());
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: bottom.x + 4,
                row: 25,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert_eq!(app.bottom_height(), 14);
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: bottom.x + 4,
                row: 25,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert!(!app.bottom_resizing());
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

    #[test]
    fn narrow_layout_renders_recovery_text_and_overlays() {
        let mut app = App::new(&[]).expect("app");
        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL))
            .expect("open file overlay");
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &app))
            .expect("draw narrow layout");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Open File"));
    }
}
