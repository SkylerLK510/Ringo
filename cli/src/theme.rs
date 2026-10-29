//! Colours for the shell. The default, `siamese`, takes its palette from a seal-point Siamese: cream and fawn fur,
//! seal-brown points, and sapphire eyes, which mark whatever is running. `siamese-light` is the same cat for light
//! terminals, `pink` blush and berry, `midnight` navy and violet, and `mono` uses no colour at all.
//!
//! The theme is picked once (RINGO_THEME, else the saved choice, else `siamese`) and fitted to the terminal once:
//! 24-bit colour where the terminal says it has it, the nearest of the 256 xterm colours otherwise, and `mono` when
//! NO_COLOR is set (https://no-color.org) or TERM is `dumb`. Drawing then costs nothing extra.

use ratatui::style::{Color, Modifier, Style};

pub const NAMES: [&str; 5] = ["siamese", "siamese-light", "pink", "midnight", "mono"];

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub name: &'static str,
    pub header: Style,
    pub brand: Style,
    pub border: Style,
    pub focus: Style,
    pub title: Style,
    pub dim: Style,
    pub selected: Style,
    pub running: Style,
    pub ok: Style,
    pub fail: Style,
    pub warn: Style,
    pub cancelled: Style,
    pub codex: Style,
    pub claude: Style,
    pub grok: Style,
    pub ringo: Style,
}

type Rgb = (u8, u8, u8);

/// A palette in 24-bit colour: header bar (fg, bg), brand, border, focus, title, dim, selected background,
/// running, ok, fail, warn, cancelled, codex, claude, grok, ringo.
struct Palette([Rgb; 17]);

const SIAMESE: Palette = Palette([
    (0xF2, 0xE8, 0xDA), (0x3B, 0x2F, 0x2A), // cream on seal brown
    (0x6F, 0xB7, 0xFF),                     // sapphire
    (0x7A, 0x66, 0x56), (0x5D, 0xA9, 0xE9), (0xEF, 0xE6, 0xD8), (0xA8, 0x95, 0x7F), (0x4A, 0x3B, 0x33),
    (0x5D, 0xA9, 0xE9), (0x94, 0xC5, 0x95), (0xE5, 0x79, 0x7A), (0xE9, 0xB8, 0x72), (0x8A, 0x7B, 0x6E),
    (0x74, 0xC7, 0xA5), (0xD9, 0x8B, 0x6A), (0xC3, 0xC9, 0xD4), (0x8C, 0xC8, 0xFF),
]);

const SIAMESE_LIGHT: Palette = Palette([
    (0x3A, 0x2C, 0x24), (0xEA, 0xDF, 0xCF),
    (0x2F, 0x6F, 0xB0),
    (0xB8, 0xA4, 0x8C), (0x2F, 0x6F, 0xB0), (0x3A, 0x2C, 0x24), (0x7D, 0x6B, 0x58), (0xE6, 0xD8, 0xC3),
    (0x2F, 0x6F, 0xB0), (0x3F, 0x7F, 0x46), (0xB2, 0x3B, 0x3B), (0xA2, 0x65, 0x0A), (0x8A, 0x7B, 0x6E),
    (0x1F, 0x8A, 0x68), (0xB3, 0x5A, 0x36), (0x5B, 0x62, 0x70), (0x2F, 0x6F, 0xB0),
]);

const PINK: Palette = Palette([
    (0xFF, 0xE4, 0xEE), (0x5A, 0x2A, 0x43), // blush on berry
    (0xFF, 0x8F, 0xC0),                     // bubblegum
    (0x8C, 0x5A, 0x73), (0xFF, 0x8F, 0xC0), (0xFF, 0xE4, 0xEE), (0xC7, 0x9A, 0xB0), (0x4A, 0x22, 0x36),
    (0xFF, 0x6F, 0xAE), (0x9B, 0xD8, 0xA8), (0xFF, 0x6B, 0x6B), (0xFF, 0xC2, 0x7A), (0x8E, 0x74, 0x80),
    (0x7F, 0xD8, 0xC3), (0xF2, 0xA0, 0x7B), (0xD6, 0xCF, 0xE0), (0xFF, 0xA6, 0xD0),
]);

const MIDNIGHT: Palette = Palette([
    (0xE6, 0xE9, 0xFF), (0x1E, 0x22, 0x40),
    (0xBB, 0x9A, 0xF7),
    (0x4B, 0x51, 0x85), (0x9D, 0x8C, 0xFF), (0xE6, 0xE9, 0xFF), (0x83, 0x89, 0xB5), (0x2A, 0x2F, 0x5A),
    (0x7A, 0xA2, 0xF7), (0x9E, 0xCE, 0x6A), (0xF7, 0x76, 0x8E), (0xE0, 0xAF, 0x68), (0x6B, 0x70, 0x94),
    (0x73, 0xDA, 0xCA), (0xFF, 0x9E, 0x64), (0xC0, 0xCA, 0xF5), (0xBB, 0x9A, 0xF7),
]);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Depth {
    TrueColor,
    Ansi256,
    Mono,
}

/// What the terminal can show, from the environment alone (no terminal query, so no startup delay).
pub fn depth(var: impl Fn(&str) -> Option<String>) -> Depth {
    if var("NO_COLOR").is_some_and(|v| !v.is_empty()) || var("TERM").as_deref() == Some("dumb") {
        return Depth::Mono;
    }
    let truecolor = var("COLORTERM").is_some_and(|v| v == "truecolor" || v == "24bit")
        || var("WT_SESSION").is_some()
        || var("TERM_PROGRAM").is_some_and(|p| matches!(p.as_str(), "iTerm.app" | "WezTerm" | "vscode" | "ghostty" | "zed"))
        || var("TERM").is_some_and(|t| t.contains("truecolor") || t.contains("24bit") || t == "xterm-kitty" || t == "alacritty");
    if truecolor { Depth::TrueColor } else { Depth::Ansi256 }
}

/// The nearest of the 256 xterm colours: the 6x6x6 cube or the 24 greys.
pub fn ansi256((r, g, b): Rgb) -> u8 {
    const LEVELS: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let near = |v: u8| (0..6).min_by_key(|&i| (LEVELS[i] - v as i32).abs()).unwrap();
    let (ri, gi, bi) = (near(r), near(g), near(b));
    let cube = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);
    let dist = |(x, y, z): (i32, i32, i32)| (x - r as i32).pow(2) + (y - g as i32).pow(2) + (z - b as i32).pow(2);
    let avg = (r as i32 + g as i32 + b as i32) / 3;
    let gi_ = ((avg - 8).max(0) / 10).min(23);
    let grey = 8 + 10 * gi_;
    if dist((grey, grey, grey)) < dist(cube) { 232 + gi_ as u8 } else { 16 + 36 * ri as u8 + 6 * gi as u8 + bi as u8 }
}

fn colour(rgb: Rgb, depth: Depth) -> Color {
    match depth {
        Depth::TrueColor => Color::Rgb(rgb.0, rgb.1, rgb.2),
        _ => Color::Indexed(ansi256(rgb)),
    }
}

impl Theme {
    pub fn named(name: &str, depth: Depth) -> Option<Theme> {
        let palette = match name {
            "siamese" => &SIAMESE,
            "siamese-light" => &SIAMESE_LIGHT,
            "pink" => &PINK,
            "midnight" => &MIDNIGHT,
            "mono" => return Some(Theme::mono()),
            _ => return None,
        };
        if depth == Depth::Mono {
            return Some(Theme::mono());
        }
        let c = |i: usize| colour(palette.0[i], depth);
        let fg = |i: usize| Style::new().fg(c(i));
        let name = NAMES.iter().find(|n| **n == name).unwrap();
        Some(Theme {
            name,
            header: Style::new().fg(c(0)).bg(c(1)),
            brand: Style::new().fg(c(2)).bg(c(1)).add_modifier(Modifier::BOLD),
            border: fg(3),
            focus: fg(4),
            title: fg(5).add_modifier(Modifier::BOLD),
            dim: fg(6),
            selected: Style::new().bg(c(7)).add_modifier(Modifier::BOLD),
            running: fg(8),
            ok: fg(9),
            fail: fg(10),
            warn: fg(11),
            cancelled: fg(12),
            codex: fg(13),
            claude: fg(14),
            grok: fg(15),
            ringo: fg(16),
        })
    }

    pub fn mono() -> Theme {
        let plain = Style::new();
        let bold = Style::new().add_modifier(Modifier::BOLD);
        Theme {
            name: "mono",
            header: Style::new().add_modifier(Modifier::REVERSED),
            brand: Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD),
            border: plain,
            focus: bold,
            title: bold,
            dim: Style::new().add_modifier(Modifier::DIM),
            selected: Style::new().add_modifier(Modifier::REVERSED),
            running: bold,
            ok: plain,
            fail: bold,
            warn: bold,
            cancelled: Style::new().add_modifier(Modifier::DIM),
            codex: plain,
            claude: plain,
            grok: plain,
            ringo: plain,
        }
    }

    /// RINGO_THEME, else the saved choice, else `siamese`; an unknown name falls back to `siamese`.
    pub fn load() -> Theme {
        let var = |k: &str| std::env::var(k).ok();
        let depth = depth(var);
        let chosen = std::env::var("RINGO_THEME").ok().or_else(saved).unwrap_or_else(|| "siamese".into());
        Theme::named(&chosen, depth).unwrap_or_else(|| Theme::named("siamese", depth).unwrap())
    }

    /// `text` with this style as ANSI escapes, for the plain-terminal setup screen; unstyled when stdout is not a
    /// terminal or the theme is mono.
    pub fn paint(&self, style: Style, text: &str) -> String {
        use std::io::IsTerminal;
        let code = match style.fg {
            Some(Color::Rgb(r, g, b)) => format!("38;2;{r};{g};{b}"),
            Some(Color::Indexed(i)) => format!("38;5;{i}"),
            _ => return text.to_string(),
        };
        if self.name == "mono" || !std::io::stdout().is_terminal() {
            return text.to_string();
        }
        let bold = if style.add_modifier.contains(Modifier::BOLD) { "1;" } else { "" };
        format!("\x1b[{bold}{code}m{text}\x1b[0m")
    }
}

fn settings() -> std::path::PathBuf {
    ringo_cli::state_root().join("shell-settings.json")
}

fn saved() -> Option<String> {
    let text = std::fs::read_to_string(settings()).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.get("theme")?.as_str().map(str::to_string)
}

/// Remembers the theme for the next start.
pub fn save(name: &str) -> Result<(), String> {
    let path = settings();
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(&path, format!("{}\n", serde_json::json!({ "theme": name }))).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_follows_the_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| {
            pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        assert_eq!(depth(env(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")])), Depth::Mono);
        assert_eq!(depth(env(&[("NO_COLOR", ""), ("COLORTERM", "truecolor")])), Depth::TrueColor);
        assert_eq!(depth(env(&[("TERM", "dumb")])), Depth::Mono);
        assert_eq!(depth(env(&[("TERM", "xterm-256color")])), Depth::Ansi256);
        assert_eq!(depth(env(&[("TERM_PROGRAM", "iTerm.app")])), Depth::TrueColor);
    }

    #[test]
    fn ansi256_picks_the_nearest_xterm_colour() {
        assert_eq!(ansi256((0, 0, 0)), 16);
        assert_eq!(ansi256((255, 255, 255)), 231);
        assert_eq!(ansi256((0x5f, 0x87, 0xaf)), 67);
        assert_eq!(ansi256((128, 128, 128)), 244);
        assert_eq!(ansi256((0x6F, 0xB7, 0xFF)), 75); // the siamese sapphire
    }

    #[test]
    fn every_named_theme_loads_and_mono_has_no_colour() {
        for name in NAMES {
            for d in [Depth::TrueColor, Depth::Ansi256, Depth::Mono] {
                let t = Theme::named(name, d).unwrap();
                if d == Depth::Mono || name == "mono" {
                    assert!(t.running.fg.is_none() && t.header.bg.is_none(), "{name}");
                }
            }
        }
        assert!(Theme::named("tabby", Depth::TrueColor).is_none());
    }
}
