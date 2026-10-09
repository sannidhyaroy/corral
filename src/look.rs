//! How Corral's pages look in a terminal: colors, bars and column widths.

use std::io::IsTerminal;

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const RED: &str = "\x1b[31m";

pub const BAR_WIDTH: usize = 20;
const PARTIAL_CELLS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];

/// Whether to draw colors and art: output goes to a terminal, `NO_COLOR` isn't set, and the terminal isn't dumb.
pub fn fancy() -> bool {
    std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
        && std::env::var("TERM").map_or(true, |term| term != "dumb")
}

/// A bar filled to `fraction` in eighth-of-a-cell steps, the fill in `color` and the rest as a dim track.
// The casts handle a fraction and at most a few hundred eighths
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn bar(fraction: f64, color: &str) -> String {
    let eighths = (fraction.clamp(0.0, 1.0) * (BAR_WIDTH * 8) as f64).round() as usize;
    let (full, partial) = (eighths / 8, eighths % 8);
    let mut out = format!("{color}{}", "█".repeat(full));
    let mut used = full;
    if partial > 0 {
        out.push(PARTIAL_CELLS[partial]);
        used += 1;
    }
    out.push_str(DIM);
    out.push_str(&"░".repeat(BAR_WIDTH - used));
    out.push_str(RESET);
    out
}

/// Columns `text` takes in a terminal: combining accents take none. Color codes must not be included.
pub fn width(text: &str) -> usize {
    text.chars().filter(|c| !('\u{300}'..='\u{36f}').contains(c)).count()
}

/// `text` followed by spaces up to `columns` terminal columns.
pub fn pad(text: &str, columns: usize) -> String {
    format!("{text}{}", " ".repeat(columns.saturating_sub(width(text))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(bar: &str) -> usize {
        let mut plain = String::new();
        let mut in_code = false;
        for c in bar.chars() {
            match c {
                '\x1b' => in_code = true,
                'm' if in_code => in_code = false,
                _ if !in_code => plain.push(c),
                _ => {}
            }
        }
        plain.chars().count()
    }

    #[test]
    fn bars_always_fill_the_full_width() {
        for percent in 0..=100 {
            assert_eq!(
                cells(&bar(f64::from(percent) / 100.0, GREEN)),
                BAR_WIDTH,
                "at {percent}%"
            );
        }
    }

    #[test]
    fn bars_use_eighth_steps() {
        assert!(bar(0.5, GREEN).starts_with(&format!("{GREEN}{}", "█".repeat(10))));
        assert!(bar(0.0125, GREEN).contains('▎'));
    }

    #[test]
    fn combining_accents_take_no_columns() {
        assert_eq!(width("(•̀ᴗ•́)و"), 6);
        assert_eq!(pad("ab", 4), "ab  ");
    }
}
