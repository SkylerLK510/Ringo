//! Compile-time Unicode half-block (`▀`) pixel art for Ringo's seal-point Siamese cat mascot.
//!
//! Packs two vertical pixels per terminal cell (`fg` = top pixel, `bg` = bottom pixel, transparent
//! background outside the silhouette) so the cat renders with crisp square pixels at 2× vertical
//! resolution across TrueColor and 256-color terminals, with zero external image crates and zero
//! startup overhead. Falls back to `mascot.txt` ASCII art automatically under `NO_COLOR` / `mono`.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::theme::{self, Depth, Theme};

/// Grid dimensions in pixels (24 cols × 20 pixel rows = 24 cols × 10 terminal rows).
pub const PIXEL_W: usize = 24;
pub const PIXEL_H: usize = 20;
pub const TERM_ROWS: usize = PIXEL_H / 2;

/// Base seal-point Siamese cat sprite (24×20 pixels, matching IMG_1878.png).
/// Palette codes:
///   '.' transparent   'K' deep seal-black   'D' dark chocolate   'M' mid seal-brown
///   'W' warm brown    'F' fawn/tan          'C' cream crown
///   'B' sapphire iris 'L' light sapphire    'P' deep pupil       'S' white catchlight
const BASE_GRID: [&str; PIXEL_H] = [
    ".KK................KK...",
    ".KDK..............KDK...",
    "..KMK............KMK....",
    "..KWMKK..CCCC..KKMWK....",
    "...KWMDKFFFFFFKDMWK.....",
    "...KWMDDMFFFFMDDMWK.....",
    "....KDMDDMMMMDDMDK......",
    "....KFFFDDKKDDFFFK......",
    "...KFFFFDKKKKDFFFFK.....",
    "...KFFMDDKKKKDDMFFK.....",
    "...KMMDBPSKKSPBDMMK.....",
    "....KMDBLPKKPLBDMK......",
    "....KDDMLLKKLLMDDK......",
    "....KDDDDKKKKDDDDK......",
    ".....KDDKKKKKKDDK.......",
    ".....KDDDKKKKDDDK.......",
    "......KDDDDDDDDK........",
    ".......KDDDDDDK.........",
    "........KMMMMK..........",
    ".........KKKK...........",
];

fn palette(ch: u8) -> Option<(u8, u8, u8)> {
    match ch {
        b'.' => None,
        b'K' => Some((22, 14, 10)),    // deep seal-brown outline & mask
        b'D' => Some((46, 30, 20)),    // dark chocolate seal
        b'M' => Some((78, 53, 35)),    // mid seal-brown
        b'W' => Some((112, 82, 57)),   // warm brown ear/cheek
        b'F' => Some((158, 130, 104)), // fawn cheek & forehead
        b'C' => Some((204, 182, 158)), // cream crown highlight
        b'B' => Some((66, 98, 122)),   // sapphire iris
        b'L' => Some((96, 134, 162)),  // light sapphire iris rim
        b'P' => Some((18, 16, 28)),    // deep indigo pupil
        b'S' => Some((244, 247, 250)), // crisp white catchlight
        b'G' => Some((148, 197, 149)), // done gleam
        b'A' => Some((233, 184, 114)), // asking amber alert
        b'R' => Some((229, 121, 122)), // failed coral accent
        _ => None,
    }
}

fn to_color(rgb: (u8, u8, u8), depth: Depth) -> Color {
    match depth {
        Depth::TrueColor => Color::Rgb(rgb.0, rgb.1, rgb.2),
        Depth::Ansi256 => Color::Indexed(theme::ansi256(rgb)),
        Depth::Mono => Color::Reset,
    }
}

fn theme_depth(t: &Theme) -> Depth {
    if t.name == "mono" {
        return Depth::Mono;
    }
    match t.brand.fg {
        Some(Color::Rgb(..)) => Depth::TrueColor,
        Some(Color::Indexed(..)) => Depth::Ansi256,
        _ => Depth::Mono,
    }
}

/// Applies subtle eye-pixel variations for each mascot state and animation frame.
fn pixel_at(state: &str, frame: usize, x: usize, y: usize) -> Option<(u8, u8, u8)> {
    let mut ch = BASE_GRID[y].as_bytes()[x];
    // Eye rows are y = 10..=11, left eye x = 7..=9, right eye x = 12..=14.
    match state {
        "idle" => {
            // Subtle blink on frame 3 if animated, otherwise open sapphire eyes.
            if frame % 8 == 7 && y == 10 && (matches!(x, 7..=9 | 12..=14)) {
                ch = b'D';
            }
        }
        "working" => {
            // Pupils subtly glance left -> center -> right -> center across frames.
            let phase = frame % 4;
            if y == 10 {
                ch = match (phase, x) {
                    (0, 7) => b'P', (0, 8) => b'S', (0, 9) => b'B',
                    (0, 12) => b'P', (0, 13) => b'S', (0, 14) => b'B',
                    (2, 7) => b'B', (2, 8) => b'S', (2, 9) => b'P',
                    (2, 12) => b'B', (2, 13) => b'S', (2, 14) => b'P',
                    _ => ch,
                };
            }
        }
        "asking" => {
            // Attentive warm amber catchlight while waiting on user approval.
            if ch == b'S' {
                ch = b'A';
            }
        }
        "done" => {
            // Bright emerald-sapphire gleam on completion.
            if ch == b'S' || ch == b'L' {
                ch = b'G';
            }
        }
        "failed" => {
            // Coral/red eye accent on failure.
            if ch == b'B' || ch == b'L' || ch == b'S' {
                ch = b'R';
            }
        }
        _ => {}
    }
    palette(ch)
}

/// Renders the pixel-art Siamese cat as `TERM_ROWS` Ratatui lines using `▀`/`▄` half-blocks.
/// Returns `None` under `mono` / `NO_COLOR` or when `max_width < PIXEL_W`, so callers fall back
/// cleanly to Ringo's text `mascot.txt`.
pub fn render(state: &str, frame: usize, max_width: usize, theme: &Theme) -> Option<Vec<Line<'static>>> {
    let depth = theme_depth(theme);
    if depth == Depth::Mono || max_width < PIXEL_W {
        return None;
    }
    let left_pad = (max_width - PIXEL_W) / 2;
    let mut lines = Vec::with_capacity(TERM_ROWS);
    for pair in 0..TERM_ROWS {
        let y_top = pair * 2;
        let y_bot = y_top + 1;
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(PIXEL_W + 1);
        if left_pad > 0 {
            spans.push(Span::raw(" ".repeat(left_pad)));
        }
        for x in 0..PIXEL_W {
            let top = pixel_at(state, frame, x, y_top);
            let bot = pixel_at(state, frame, x, y_bot);
            let span = match (top, bot) {
                (None, None) => Span::raw(" "),
                (Some(t), None) => Span::styled("▀", Style::new().fg(to_color(t, depth))),
                (None, Some(b)) => Span::styled("▄", Style::new().fg(to_color(b, depth))),
                (Some(t), Some(b)) if t == b => Span::styled("█", Style::new().fg(to_color(t, depth))),
                (Some(t), Some(b)) => Span::styled("▀", Style::new().fg(to_color(t, depth)).bg(to_color(b, depth))),
            };
            spans.push(span);
        }
        lines.push(Line::from(spans));
    }
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_dimensions_are_exact_and_fit_side_panel() {
        assert_eq!(BASE_GRID.len(), PIXEL_H);
        for (i, row) in BASE_GRID.iter().enumerate() {
            assert_eq!(row.len(), PIXEL_W, "row {i} width mismatch");
        }
        assert!(PIXEL_W <= 28, "must fit inside PANEL_W (32) with borders");
    }

    #[test]
    fn renders_for_truecolor_and_ansi256_and_falls_back_on_mono() {
        let tc = Theme::named("siamese", Depth::TrueColor).unwrap();
        let a256 = Theme::named("siamese", Depth::Ansi256).unwrap();
        let mono = Theme::named("mono", Depth::Mono).unwrap();

        for state in ["idle", "working", "asking", "done", "failed"] {
            let lines = render(state, 0, 28, &tc).expect("truecolor renders");
            assert_eq!(lines.len(), TERM_ROWS);
            let lines_256 = render(state, 2, 28, &a256).expect("ansi256 renders");
            assert_eq!(lines_256.len(), TERM_ROWS);
        }
        assert!(render("idle", 0, 28, &mono).is_none(), "mono falls back to ASCII mascot");
        assert!(render("idle", 0, 16, &tc).is_none(), "narrow pane falls back cleanly");
    }
}
