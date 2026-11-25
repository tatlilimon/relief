//! The parts that need a real terminal: the picker's key loop and the progress spinner.

use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute, queue};

use crate::picker::{Key, Picker, Row, Step};
use crate::style::Style;

/// Width of the terminal standard output is attached to, if it is one.
pub fn terminal_width() -> Option<usize> {
    if !io::stdout().is_terminal() {
        return None;
    }
    Some(size().0)
}

/// Columns and rows of the terminal. Some terminals report nothing, or zero; assume the
/// classic 80 by 24 then, and never less than a usable minimum.
fn size() -> (usize, usize) {
    let (width, height) = terminal::size().unwrap_or((0, 0));
    let or = |value: u16, default: usize, least: usize| {
        if value == 0 {
            default
        } else {
            (value as usize).max(least)
        }
    };
    (or(width, 80, 40), or(height, 24, 8))
}

/// Puts the terminal back however the picker ends.
struct Screen;

impl Screen {
    fn open() -> io::Result<Screen> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
        Ok(Screen)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), cursor::Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Option<Key> {
    Some(match code {
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => Key::Cancel,
        KeyCode::Up | KeyCode::Char('k') => Key::Up,
        KeyCode::Down | KeyCode::Char('j') => Key::Down,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Home | KeyCode::Char('g') => Key::First,
        KeyCode::End | KeyCode::Char('G') => Key::Last,
        KeyCode::Char(' ') => Key::Toggle,
        KeyCode::Char('a') => Key::All,
        KeyCode::Char('n') => Key::Nothing,
        KeyCode::Char('p') => Key::Projects,
        KeyCode::Char('c') => Key::Caches,
        KeyCode::Char('d') => Key::Docker,
        KeyCode::Enter => Key::Confirm,
        KeyCode::Esc | KeyCode::Char('q') => Key::Cancel,
        _ => return None,
    })
}

/// Let the user tick rows. `None` when they cancel.
pub fn pick(rows: Vec<Row>, style: &Style) -> io::Result<Option<Vec<usize>>> {
    let mut picker = Picker::new(rows);
    let _screen = Screen::open()?;
    let mut out = io::stdout();
    loop {
        let (width, height) = size();
        let style = Style {
            width: Some(width),
            ..*style
        };
        queue!(
            out,
            cursor::MoveTo(0, 0),
            terminal::Clear(terminal::ClearType::All)
        )?;
        for line in picker.frame(height, &style) {
            // Raw mode does not turn a newline into a carriage return.
            write!(out, "{line}\r\n")?;
        }
        out.flush()?;
        let Event::Key(press) = event::read()? else {
            continue;
        };
        if press.kind != KeyEventKind::Press {
            continue;
        }
        let page = height.saturating_sub(5).max(1);
        match key(press.code, press.modifiers).map(|k| picker.press(k, page)) {
            Some(Step::Confirm(chosen)) => return Ok(Some(chosen)),
            Some(Step::Cancel) => return Ok(None),
            _ => {}
        }
    }
}

/// A sign of life on standard error while a slow step runs. Silent when nobody is watching.
pub struct Spinner {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Spinner {
    pub fn start(message: &'static str) -> Spinner {
        let stop = Arc::new(AtomicBool::new(false));
        if !io::stderr().is_terminal() {
            return Spinner { stop, thread: None };
        }
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let mut i = 0;
            while !flag.load(Ordering::Relaxed) {
                eprint!("\r{} {message}", frames[i % frames.len()]);
                let _ = io::stderr().flush();
                i += 1;
                std::thread::sleep(Duration::from_millis(80));
            }
            // Wipe the line so the report starts clean.
            eprint!("\r{}\r", " ".repeat(message.chars().count() + 2));
            let _ = io::stderr().flush();
        });
        Spinner {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
