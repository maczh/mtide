mod app;
mod config;
mod core;
mod error;
mod git;
mod lsp;
mod plugin;
mod search;
mod terminal;
mod ui;

use std::io::{self, IsTerminal, Read, Write};

use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use app::App;
use error::AppResult;

fn main() -> AppResult<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        print_help();
        return Ok(());
    }
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--version" | "-v"))
    {
        println!("mtide {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let has_explicit_target = args.iter().enumerate().any(|(index, arg)| {
        if arg == "-" {
            return true;
        }
        !arg.starts_with('-')
            && (index == 0
                || !matches!(
                    args.get(index - 1).map(String::as_str),
                    Some("--config" | "--plugin" | "--workspace")
                ))
    });
    let welcome = args.iter().any(|arg| arg == "--welcome");
    let stdin_text = if args.iter().any(|arg| arg == "-")
        || (!welcome && !has_explicit_target && !io::stdin().is_terminal())
    {
        let mut text = String::new();
        io::stdin().read_to_string(&mut text)?;
        Some(text)
    } else {
        None
    };
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, &args, stdin_text.as_deref());

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        crossterm::event::DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    result
}

fn run<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    args: &[String],
    stdin_text: Option<&str>,
) -> AppResult<()> {
    let mut app = match stdin_text {
        Some(text) => App::new_with_stdin(args, Some(text))?,
        None => App::new(args)?,
    };
    loop {
        app.poll_background();
        if !app.should_quit() {
            let size = terminal.size()?;
            for mouse in app.drain_plugin_mouse_events() {
                ui::handle_mouse(
                    &mut app,
                    mouse,
                    ratatui::layout::Rect::new(0, 0, size.width, size.height),
                );
            }
        }
        terminal.draw(|frame| ui::draw(frame, &app))?;
        if event::poll(std::time::Duration::from_millis(250))? {
            let input = event::read()?;
            match input {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let should_quit = handle_key(&mut app, key);
                    if let Some(clipboard) = app.take_osc52_clipboard() {
                        write_osc52_clipboard(&clipboard)?;
                    }
                    if should_quit {
                        break;
                    }
                }
                Event::Paste(text) => {
                    if let Err(error) = app.handle_paste(&text) {
                        app.set_status_message(format!("Paste error: {error}"));
                    }
                }
                Event::Resize(columns, rows) => {
                    if let Err(error) = app.resize_terminal(columns, rows) {
                        app.set_status_message(format!("Resize error: {error}"));
                    }
                }
                Event::Mouse(mouse) => {
                    let size = terminal.size()?;
                    ui::handle_mouse(
                        &mut app,
                        mouse,
                        ratatui::layout::Rect::new(0, 0, size.width, size.height),
                    );
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn print_help() {
    println!(
        "mtide {}\n\nUSAGE:\n    mtide [OPTIONS] [FILE|DIRECTORY]\n\nOPTIONS:\n    --plugin DIR      Load a TTT-compatible Lua plugin package (repeatable)\n    --workspace FILE  Open a .ttt workspace\n    --config FILE     Load settings from FILE\n    --welcome         Start with a welcome buffer\n    -h, --help        Show this help\n    --version, -v     Show version\n\nKEYS:\n    Ctrl-Q         Quit\n    Tab             Cycle focus\n    Ctrl-B/T/A      Toggle sidebar/terminal/agent\n    Ctrl-P          Plugin command palette",
        env!("CARGO_PKG_VERSION")
    );
}

fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    if key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.quit();
        return true;
    }
    if let Err(error) = app.handle_key(key) {
        app.set_status_message(format!("Error: {error}"));
    }
    app.should_quit()
}

const OSC52_MAX_BYTES: usize = 64 * 1024;

fn write_osc52_clipboard(text: &str) -> AppResult<()> {
    let Some(payload) = osc52_payload(text) else {
        return Ok(());
    };
    let mut stdout = io::stdout();
    stdout.write_all(format!("\x1b]52;c;{payload}\x07").as_bytes())?;
    stdout.flush()?;
    Ok(())
}

fn osc52_payload(text: &str) -> Option<String> {
    if text.len() > OSC52_MAX_BYTES {
        return None;
    }
    let bytes = text.as_bytes();
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        encoded.push(ALPHABET[((first & 0b11) << 4 | second >> 4) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[((second & 0b1111) << 2 | third >> 6) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[(third & 0b111111) as usize] as char
        } else {
            '='
        });
    }
    Some(encoded)
}

#[cfg(test)]
mod tests {
    use super::App;

    #[test]
    fn package_metadata_is_available_for_cli() {
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }

    #[test]
    fn stdin_target_is_read_as_an_unsaved_buffer() {
        let args = vec!["-".to_owned()];
        let app = App::new_with_stdin(&args, Some("hello\nworld\n")).expect("stdin app");
        assert_eq!(app.file_lines(), &["hello", "world", ""]);
    }

    #[test]
    fn piped_stdin_without_a_target_is_read_as_an_unsaved_buffer() {
        let app = App::new_with_stdin(&[], Some("piped")).expect("stdin app");
        assert_eq!(app.file_lines(), &["piped"]);
    }

    #[test]
    fn welcome_flag_suppresses_piped_stdin() {
        let args = vec!["--welcome".to_owned()];
        let app = App::new_with_stdin(&args, Some("piped")).expect("welcome app");
        assert_eq!(app.file_lines()[0], "# mtide");
    }

    #[test]
    fn osc52_payload_uses_bounded_base64_encoding() {
        assert_eq!(super::osc52_payload("hello"), Some("aGVsbG8=".to_owned()));
        assert_eq!(super::osc52_payload("你好"), Some("5L2g5aW9".to_owned()));
        assert!(super::osc52_payload(&"x".repeat(super::OSC52_MAX_BYTES + 1)).is_none());
    }
}
