//! How output looks: colour when a person is watching, plain text otherwise.

use crate::env::Env;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ColorChoice {
    /// Colour when writing to a terminal, unless NO_COLOR is set
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub color: bool,
    /// Columns available; `None` when output is not a terminal and lines may be any length.
    pub width: Option<usize>,
}

impl Style {
    pub const PLAIN: Style = Style {
        color: false,
        width: None,
    };

    /// Decide from the user's choice, the environment and whether a terminal is attached.
    pub fn decide(choice: ColorChoice, env: &Env, terminal: Option<usize>) -> Style {
        let color = match choice {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => {
                terminal.is_some()
                    && env.var("NO_COLOR").is_none()
                    && env.var("TERM") != Some("dumb")
            }
        };
        Style {
            color,
            width: terminal,
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }
    pub fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }
    pub fn accent(&self, text: &str) -> String {
        self.paint("36", text)
    }
    pub fn good(&self, text: &str) -> String {
        self.paint("32", text)
    }
    pub fn warn(&self, text: &str) -> String {
        self.paint("33", text)
    }
    pub fn bad(&self, text: &str) -> String {
        self.paint("31", text)
    }
}

/// Number of terminal columns a string takes, ignoring colour codes.
pub fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip a colour sequence: ESC [ ... m
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            width += 1;
        }
    }
    width
}

/// Shorten text to `max` columns by cutting out its middle, keeping the end, which for a
/// path is the part that tells entries apart.
pub fn fit(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let head = (max - 1) / 3;
    let tail = max - 1 - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> (tempfile::TempDir, Env) {
        let dir = tempfile::tempdir().unwrap();
        let mut vars: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        vars.insert("HOME".into(), dir.path().display().to_string());
        let env = Env::from_vars(vars, 0).unwrap();
        (dir, env)
    }

    #[test]
    fn colour_is_for_terminals_only_unless_forced() {
        let (_d, e) = env(&[("TERM", "xterm")]);
        assert!(Style::decide(ColorChoice::Auto, &e, Some(80)).color);
        assert!(!Style::decide(ColorChoice::Auto, &e, None).color);
        assert!(Style::decide(ColorChoice::Always, &e, None).color);
        assert!(!Style::decide(ColorChoice::Never, &e, Some(80)).color);
    }

    #[test]
    fn no_color_and_dumb_terminals_are_respected() {
        let (_d, e) = env(&[("TERM", "xterm"), ("NO_COLOR", "1")]);
        assert!(!Style::decide(ColorChoice::Auto, &e, Some(80)).color);
        let (_d, e) = env(&[("TERM", "dumb")]);
        assert!(!Style::decide(ColorChoice::Auto, &e, Some(80)).color);
    }

    #[test]
    fn the_width_is_the_terminal_width() {
        let (_d, e) = env(&[]);
        assert_eq!(
            Style::decide(ColorChoice::Auto, &e, Some(72)).width,
            Some(72)
        );
        assert_eq!(Style::decide(ColorChoice::Always, &e, None).width, None);
    }

    #[test]
    fn plain_text_carries_no_escape_codes() {
        let colored = Style {
            color: true,
            width: None,
        };
        assert_eq!(Style::PLAIN.bold("x"), "x");
        assert_eq!(colored.bold("x"), "\x1b[1mx\x1b[0m");
        assert_eq!(visible_width(&colored.bad("héllo")), 5);
    }

    #[test]
    fn long_text_loses_its_middle_and_keeps_its_end() {
        let path = "~/code/clients/acme/archive/analytics/frontend/node_modules";
        let short = fit(path, 40);
        assert_eq!(visible_width(&short), 40);
        assert!(short.starts_with("~/cod"), "{short}");
        assert!(short.ends_with("frontend/node_modules"), "{short}");
        assert!(short.contains('…'));
        assert_eq!(fit("~/a/b", 40), "~/a/b");
        assert_eq!(visible_width(&fit(path, 5)), 5);
    }
}
