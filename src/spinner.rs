//! The braille "columns" loader, ported from Kyber Studio's `Spinner.tsx`
//! (itself from Eve Studio, adapted from `gunnargray-dev/unicode-animations`).
//!
//! Frames are generated rather than listed: fill each of six columns
//! bottom-up, then flash full and empty. A generated sequence cannot drift out
//! of order the way a hand-typed frame list does, and braille is a better fit
//! here than in a browser — a braille cell *is* a terminal cell, two dots wide
//! by four tall, so the animation renders at native resolution instead of
//! being approximated.
//!
//! The frame is a pure function of elapsed time rather than a counter someone
//! has to advance. Nothing owns the animation, so nothing can forget to tick
//! it, and two spinners on screen stay in phase.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Dot bits by (row, half-column), per the Unicode braille block.
const DOT_MAP: [[u32; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];
/// Grid width in dots. Six dots is three braille cells.
const W: usize = 6;
/// Grid height in dots — the full height of a braille cell.
const H: usize = 4;
/// Milliseconds per frame, matching Kyber Studio so the two feel identical.
const FRAME_MS: u128 = 60;

/// Columns of the rendered spinner. Callers reserve this much width.
pub const WIDTH: usize = W.div_ceil(2);

fn to_braille(grid: &[[bool; W]; H]) -> String {
    let mut out = String::with_capacity(WIDTH * 3);
    for cell in 0..WIDTH {
        let mut code = 0x2800u32;
        for (row, dots) in DOT_MAP.iter().enumerate() {
            for (half, bit) in dots.iter().enumerate() {
                let column = cell * 2 + half;
                if column < W && grid[row][column] {
                    code |= bit;
                }
            }
        }
        out.push(char::from_u32(code).unwrap_or(' '));
    }
    out
}

fn frames() -> &'static [String] {
    static FRAMES: OnceLock<Vec<String>> = OnceLock::new();
    FRAMES.get_or_init(|| {
        let mut frames = Vec::new();
        for column in 0..W {
            // Each column fills from the bottom up, over the columns already
            // filled to its left.
            for fill_to in (0..H).rev() {
                let mut grid = [[false; W]; H];
                for filled in 0..column {
                    for row in grid.iter_mut() {
                        row[filled] = true;
                    }
                }
                for row in grid.iter_mut().skip(fill_to) {
                    row[column] = true;
                }
                frames.push(to_braille(&grid));
            }
        }
        frames.push(to_braille(&[[true; W]; H]));
        frames.push(to_braille(&[[false; W]; H]));
        frames
    })
}

fn origin() -> Instant {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    *ORIGIN.get_or_init(Instant::now)
}

/// The frame for right now.
pub fn frame() -> &'static str {
    frame_at(origin().elapsed())
}

fn frame_at(elapsed: Duration) -> &'static str {
    let frames = frames();
    let index = (elapsed.as_millis() / FRAME_MS) as usize % frames.len();
    &frames[index]
}

/// How often a caller must repaint for the animation to look continuous.
pub fn tick() -> Duration {
    Duration::from_millis(FRAME_MS as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn every_frame_is_the_same_width() {
        // A spinner that changes width shifts the text beside it on every
        // frame, which reads as the label stuttering rather than as motion.
        for frame in frames() {
            assert_eq!(frame.width(), WIDTH, "{frame:?}");
            assert_eq!(frame.chars().count(), WIDTH);
        }
    }

    #[test]
    fn every_frame_is_inside_the_braille_block() {
        for frame in frames() {
            for glyph in frame.chars() {
                let code = glyph as u32;
                assert!(
                    (0x2800..=0x28ff).contains(&code),
                    "{glyph:?} is not braille"
                );
            }
        }
    }

    #[test]
    fn the_sequence_fills_column_by_column_then_flashes() {
        // The shape of the animation: the first column lights one dot at the
        // bottom, columns fill left to right, then it flashes full and clears.
        // A generated sequence that never fills is a bug in the generator
        // rather than one visibly wrong frame.
        let all = frames();
        assert_eq!(
            all.first().map(String::as_str),
            Some("⡀⠀⠀"),
            "starts bottom-left"
        );
        assert!(all.iter().any(|frame| frame == "⣿⣿⣿"), "never fills");
        assert_eq!(all.last().map(String::as_str), Some("⠀⠀⠀"), "ends clear");
        assert_eq!(all.len(), W * H + 2);
    }

    #[test]
    fn the_frame_advances_with_time_and_wraps() {
        let count = frames().len() as u64;
        assert_eq!(frame_at(Duration::ZERO), frames()[0]);
        assert_eq!(frame_at(Duration::from_millis(60)), frames()[1]);
        // A long-running client must not index past the end.
        assert_eq!(
            frame_at(Duration::from_millis(60 * count)),
            frames()[0],
            "the sequence must wrap"
        );
        assert_eq!(frame_at(Duration::from_secs(86_400)).width(), WIDTH);
    }
}
