//! The key reference.
//!
//! Everything the app can do lives here rather than on the composer's border.
//! A hint strip has room for about four bindings before it stops being read at
//! all, so it keeps the ones needed to send a message and this carries the
//! rest — including the mouse, which a strip cannot express.

/// A group of related bindings.
pub struct Section {
    pub title: &'static str,
    pub rows: Vec<(String, &'static str)>,
}

fn section(title: &'static str, rows: Vec<(&str, &'static str)>) -> Section {
    Section {
        title,
        rows: rows
            .into_iter()
            .map(|(keys, what)| (keys.to_string(), what))
            .collect(),
    }
}

/// The full reference.
///
/// `newline` is passed in because which key produces one depends on what the
/// terminal can report — printing a binding that does nothing here would be
/// worse than omitting it.
pub fn sections(newline: &str) -> Vec<Section> {
    vec![
        section(
            "WRITING",
            vec![
                ("⏎", "send"),
                (newline, "newline"),
                ("^J", "newline, on any terminal"),
                ("@", "mention someone"),
                ("^W", "delete the last word"),
                ("^U", "clear the draft"),
            ],
        ),
        section(
            "MOVING AROUND",
            vec![
                ("⇥  ⇧⇥", "next / previous channel"),
                ("^F", "search every channel"),
                ("^T", "open the most recent thread"),
                ("esc", "close a thread, popup, or search"),
                ("PgUp  PgDn", "scroll a page"),
                ("^↑  ^↓", "scroll a line"),
            ],
        ),
        section(
            "CONVERSATIONS",
            vec![
                ("^K", "switch community"),
                ("^N", "start a direct message with anyone"),
                ("^X", "hide or restore a direct message"),
                ("^R", "reveal hidden direct messages"),
                ("^E", "react to the newest message"),
                ("^G", "the channel's shared canvas"),
            ],
        ),
        section(
            "MOUSE",
            vec![
                ("click a channel", "open it"),
                ("click a name", "react to that message"),
                ("click a reaction", "add or take yours back"),
                ("click N replies", "open that thread"),
                ("wheel", "scrolls the pane under the pointer"),
            ],
        ),
        section(
            "ELSEWHERE",
            vec![
                ("^E", "edit the canvas in $EDITOR (in the canvas)"),
                ("^C", "quit"),
                ("--probe", "diagnose a blank channel, from the shell"),
            ],
        ),
    ]
}

/// Widest key column across every section, so the descriptions line up.
pub fn key_column(sections: &[Section]) -> usize {
    sections
        .iter()
        .flat_map(|section| section.rows.iter())
        .map(|(keys, _)| keys.chars().count())
        .max()
        .unwrap_or(0)
}

/// Rendered height, sections included.
pub fn height(sections: &[Section]) -> usize {
    sections
        .iter()
        .map(|section| section.rows.len() + 2)
        .sum::<usize>()
        .saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_says_what_the_key_does() {
        for section in sections("⇧⏎") {
            assert!(!section.title.is_empty());
            for (keys, what) in &section.rows {
                assert!(!keys.is_empty(), "{} has a blank key", section.title);
                assert!(!what.is_empty(), "{keys} has no description");
            }
        }
    }

    #[test]
    fn the_newline_key_is_whatever_the_terminal_can_deliver() {
        // The help must not tell someone to press a key their terminal cannot
        // distinguish from Enter.
        let alt = sections("⌥⏎");
        assert!(alt
            .iter()
            .flat_map(|section| section.rows.iter())
            .any(|(keys, _)| keys == "⌥⏎"));
    }

    #[test]
    fn the_key_column_fits_the_widest_binding() {
        let all = sections("⇧⏎");
        let width = key_column(&all);
        for section in &all {
            for (keys, _) in &section.rows {
                assert!(keys.chars().count() <= width);
            }
        }
    }
}
