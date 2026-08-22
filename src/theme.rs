//! The palette and the small set of styles built from it.
//!
//! Two accents carry meaning and nothing else borrows them:
//!
//! - **Cyan** is structure and focus — the selected channel, the compose caret,
//!   the channel name in the header. If it is cyan, it is where you are.
//! - **Emerald** is liveness and self — a healthy connection, and your own name
//!   in the transcript. If it is emerald, it is working or it is you.
//!
//! Author colors deliberately avoid both, so a participant who happens to hash
//! into cyan never reads as "selected". Everything else is one of four
//! neutrals, which is what keeps two accents legible.
//!
//! Colors are 24-bit. Every terminal Buzz Desktop supports handles truecolor,
//! and the alternative — the 16-color cube — cannot express the difference
//! between a rule and a background that this layout depends on.

use ratatui::style::{Color, Modifier, Style};

// ── accents ────────────────────────────────────────────────────────────────
/// Structure and focus.
pub const CYAN: Color = Color::Rgb(0x22, 0xd3, 0xee);
/// Cyan at rest — section labels, inactive structure.
pub const CYAN_DIM: Color = Color::Rgb(0x0e, 0x8b, 0xa8);
/// Liveness and self.
pub const EMERALD: Color = Color::Rgb(0x34, 0xd3, 0x99);
/// Emerald at rest — today's date separator, which is the only structural
/// element that means "now" rather than "here".
pub const EMERALD_DIM: Color = Color::Rgb(0x05, 0x96, 0x69);

// ── state ──────────────────────────────────────────────────────────────────
/// Connecting, and other "not yet wrong" states.
pub const AMBER: Color = Color::Rgb(0xf5, 0xb7, 0x4b);
/// Disconnected, failed sends.
pub const ROSE: Color = Color::Rgb(0xf4, 0x7c, 0x8c);

// ── neutrals ───────────────────────────────────────────────────────────────
/// Message bodies.
pub const TEXT: Color = Color::Rgb(0xd6, 0xdb, 0xe4);
/// Secondary text — topics, timestamps, notices.
pub const MUTED: Color = Color::Rgb(0x88, 0x91, 0xa3);
/// Tertiary text — placeholders, hints, day separators.
pub const FAINT: Color = Color::Rgb(0x56, 0x5f, 0x72);
/// Hairlines. A rule should separate rather than divide, but the first pass
/// sat so close to a dark terminal ground that the boxes read as smudges. This
/// is the quietest value that still resolves as a line.
pub const RULE: Color = Color::Rgb(0x3d, 0x47, 0x60);
/// The frame around a fenced code block. Brighter than a pane border because
/// it sits *inside* a message, competing with body text rather than with the
/// terminal background — at RULE it disappeared entirely.
pub const CODE_FRAME: Color = Color::Rgb(0x5c, 0x6a, 0x86);
/// Selected-row ground.
pub const SELECTED_BG: Color = Color::Rgb(0x15, 0x1f, 0x2c);
/// Code ground. Lifted off the terminal background rather than darkened, so a
/// code block reads as raised and still works on a light terminal.
pub const CODE_BG: Color = Color::Rgb(0x18, 0x22, 0x30);

/// Author colors, in a family that reads as one set and stays clear of both
/// accents. Sky and teal sit next to cyan without being it, which is the point:
/// a room of eight people should look like a palette, not a fruit bowl.
pub const AUTHORS: [Color; 8] = [
    Color::Rgb(0x7d, 0xd3, 0xfc), // sky
    Color::Rgb(0xa7, 0x8b, 0xfa), // violet
    Color::Rgb(0xf5, 0xb7, 0x4b), // amber
    Color::Rgb(0x5e, 0xea, 0xd4), // teal
    Color::Rgb(0xf4, 0x8f, 0xb1), // rose
    Color::Rgb(0xa3, 0xe6, 0x35), // lime
    Color::Rgb(0x93, 0xb4, 0xff), // periwinkle
    Color::Rgb(0xd8, 0xb4, 0xfe), // orchid
];

/// Deterministic per-author color, stable across restarts.
pub fn author(pubkey_hex: &str, is_me: bool) -> Color {
    if is_me {
        return EMERALD;
    }
    let sum: u32 = pubkey_hex.bytes().map(u32::from).sum();
    AUTHORS[(sum as usize) % AUTHORS.len()]
}

pub fn body() -> Style {
    Style::default().fg(TEXT)
}

pub fn muted() -> Style {
    Style::default().fg(MUTED)
}

pub fn faint() -> Style {
    Style::default().fg(FAINT)
}

pub fn rule() -> Style {
    Style::default().fg(RULE)
}

/// Inline `code` — a tinted ground rather than a color change, so a code span
/// inside a sentence stays part of the sentence.
pub fn inline_code() -> Style {
    Style::default().fg(CYAN).bg(CODE_BG)
}

/// Code inside a fenced block.
pub fn code() -> Style {
    Style::default().fg(TEXT).bg(CODE_BG)
}

/// The frame around a fenced code block.
pub fn code_frame() -> Style {
    Style::default().fg(CODE_FRAME)
}

/// The language label on a fenced block. It names the content, so it reads at
/// body weight rather than as chrome.
pub fn code_label() -> Style {
    Style::default().fg(MUTED).add_modifier(Modifier::BOLD)
}

/// Link text. Underlined as well as colored, because color alone is not a
/// signal for every reader.
pub fn link() -> Style {
    Style::default().fg(CYAN).add_modifier(Modifier::UNDERLINED)
}

/// A reaction pill someone else left.
pub fn pill() -> Style {
    Style::default().fg(MUTED).bg(CODE_BG)
}

/// A reaction pill you left. Cyan is "you are here" everywhere else in the
/// layout, and on a pill it means the click that takes it back.
pub fn pill_mine() -> Style {
    Style::default().fg(CYAN).bg(SELECTED_BG)
}

/// Pane names on a border.
pub fn pane_title() -> Style {
    Style::default().fg(CYAN_DIM).add_modifier(Modifier::BOLD)
}

/// A key glyph in the binding hints — bright enough to find, quiet enough to
/// ignore once learned.
pub fn key() -> Style {
    Style::default().fg(MUTED).add_modifier(Modifier::BOLD)
}

/// The selected channel's text. The background is applied to the whole line
/// rather than here, so the highlight spans the row.
pub fn channel_selected() -> Style {
    Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
}

pub fn channel_unread() -> Style {
    Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
}

pub fn channel_idle() -> Style {
    Style::default().fg(MUTED)
}

/// Unread count. A filled emerald pill is the one badge in the layout, so it
/// is the only thing competing with the cyan selection — and it wins only when
/// the row is not selected, where the count is redundant anyway.
pub fn badge() -> Style {
    Style::default().fg(EMERALD).add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn my_own_name_is_always_emerald() {
        // Self-identification must not depend on a hash: "which one am I" is
        // the question the transcript is asked most often.
        assert_eq!(author("00", true), EMERALD);
    }

    #[test]
    fn author_colors_avoid_both_accents() {
        // A participant who hashed into cyan would read as the selected
        // channel's color, and one in emerald as "you".
        assert!(!AUTHORS.contains(&CYAN));
        assert!(!AUTHORS.contains(&EMERALD));
    }

    #[test]
    fn author_colors_are_stable_for_a_key() {
        let key = "57f3d82600303492c8f320f0801291423d053bcd328344bcef6092f376445b04";
        assert_eq!(author(key, false), author(key, false));
    }
}
