//! The reaction palette.
//!
//! A curated set rather than the whole of Unicode: a picker you scroll is
//! slower than typing the name of the one you wanted, and in a chat the same
//! dozen reactions cover nearly everything. Each entry carries a searchable
//! name, so `:tada:` and `party` both reach 🎉.
//!
//! Custom workspace emoji (`kind:30030`) are deliberately absent. A terminal
//! cannot render the image a shortcode points at, so offering them would let
//! someone send a reaction that shows here as bare `:shortcode:` text — and
//! incoming ones already render that way, which is the honest limit.

/// `(searchable name, glyph)`, in the order the picker shows them.
const PALETTE: &[(&str, &str)] = &[
    ("+1 thumbsup yes approve", "👍"),
    ("-1 thumbsdown no", "👎"),
    ("eyes looking watching", "👀"),
    ("white_check_mark done check ok", "✅"),
    ("x cross no fail", "❌"),
    ("tada party celebrate ship", "🎉"),
    ("rocket ship deploy launch", "🚀"),
    ("fire hot burn", "🔥"),
    ("heart love", "❤️"),
    ("laughing joy funny haha", "😄"),
    ("thinking hmm consider", "🤔"),
    ("pray thanks please", "🙏"),
    ("clap applause nice", "👏"),
    ("wave hello hi bye", "👋"),
    ("100 perfect exactly", "💯"),
    ("warning careful caution", "⚠️"),
    ("bug broken defect", "🐛"),
    ("sad cry disappointed", "😢"),
];

/// The palette, as `(name, glyph)` pairs.
pub fn palette() -> &'static [(&'static str, &'static str)] {
    PALETTE
}

/// The label shown for an entry: its first, canonical name.
pub fn label(names: &str) -> &str {
    names.split_whitespace().next().unwrap_or(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn every_entry_is_searchable_by_more_than_one_word() {
        // The point of the name list: nobody remembers that 🎉 is "tada".
        for (names, _) in palette() {
            assert!(
                names.split_whitespace().count() > 1,
                "{names:?} has no aliases"
            );
        }
    }

    #[test]
    fn the_palette_has_no_duplicate_glyphs() {
        // Two entries producing the same reaction would look like a picker bug.
        let mut seen = std::collections::HashSet::new();
        for (_, glyph) in palette() {
            assert!(seen.insert(*glyph), "{glyph} appears twice");
        }
    }

    #[test]
    fn no_glyph_is_wider_than_a_pill_expects() {
        // Pill widths are measured, but a glyph wider than two cells would
        // make every row in the picker ragged.
        for (_, glyph) in palette() {
            assert!(glyph.width() <= 2, "{glyph} is {} cells", glyph.width());
        }
    }

    #[test]
    fn the_label_is_the_first_name() {
        assert_eq!(label("tada party celebrate ship"), "tada");
        assert_eq!(label("eyes"), "eyes");
    }
}
