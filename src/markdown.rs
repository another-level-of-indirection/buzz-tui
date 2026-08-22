//! GitHub-flavored Markdown to styled terminal lines.
//!
//! Buzz agents write formatted messages — headings, bullet lists, fenced code,
//! tables — so a chat client that renders the asterisks is showing the reader
//! the wrong thing. The desktop client uses `remark-gfm`; this is the terminal
//! equivalent of the same target.
//!
//! Rendering is to `Vec<Line>` rather than to a widget, so the transcript keeps
//! owning scroll, grouping, and day separators. Wrapping happens here rather
//! than in `Paragraph` because it has to survive styling: a bold run that
//! crosses a line break must stay bold on both lines, which a post-hoc wrap of
//! finished spans cannot do.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::theme;

/// Columns a nested list level indents by.
const LIST_INDENT: usize = 2;
/// Widest a rendered table may get before columns start being squeezed.
const MIN_COLUMN: usize = 3;

/// One run of text sharing a style, before wrapping.
#[derive(Clone)]
struct Seg {
    text: String,
    style: Style,
    /// Index into [`Rendered::urls`] when this run is part of a link.
    link: Option<usize>,
}

/// Where a link landed after wrapping, in rendered-line coordinates.
///
/// A link that wraps produces one of these per line: a terminal has no notion
/// of a shape spanning rows, so each fragment is its own click target.
#[derive(Clone, Debug)]
pub struct LinkSpan {
    pub line: usize,
    pub start: usize,
    pub end: usize,
    pub url: usize,
}

/// Rendered Markdown, plus where its links ended up.
#[derive(Default)]
pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    pub links: Vec<LinkSpan>,
    pub urls: Vec<String>,
}

/// Renders `content` into lines at most `width` columns wide.
pub fn render(content: &str, width: usize) -> Rendered {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_SMART_PUNCTUATION);
    // Blockquote alerts. Note this does *not* bring GFM autolink literals —
    // pulldown-cmark only autolinks the CommonMark `<url>` form — so bare URLs
    // are found in `text` instead.
    options.insert(Options::ENABLE_GFM);

    let mut renderer = Renderer::new(width.max(8));
    for event in Parser::new_ext(content, options) {
        renderer.event(event);
    }
    renderer.finish()
}

struct Renderer {
    width: usize,
    out: Vec<Line<'static>>,
    links: Vec<LinkSpan>,
    urls: Vec<String>,
    /// The link currently open, if any.
    link: Option<usize>,
    /// The inline run being accumulated for the current block.
    segs: Vec<Seg>,
    /// Plain text seen since the last non-text event.
    ///
    /// Buffered rather than scanned per event because the parser splits a text
    /// run at anything that *might* be an emphasis delimiter — so
    /// `https://example.com/a_(b)` arrives in pieces, and a scanner looking at
    /// one piece finds a URL ending at the underscore.
    pending: String,
    styles: Vec<Style>,
    /// One entry per open list. `Some(n)` is an ordered list's next number.
    lists: Vec<Option<u64>>,
    quotes: usize,
    /// Set between a list item's start and its first block, so the bullet is
    /// attached to that block rather than emitted on a line of its own.
    pending_marker: Option<String>,
    code: Option<CodeBlock>,
    table: Option<Table>,
}

struct CodeBlock {
    language: String,
    lines: Vec<String>,
}

struct Table {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    /// Cells accumulate here until the row ends.
    row: Vec<String>,
    cell: String,
    in_header: bool,
}

impl Renderer {
    fn new(width: usize) -> Self {
        Self {
            width,
            out: Vec::new(),
            links: Vec::new(),
            urls: Vec::new(),
            link: None,
            segs: Vec::new(),
            pending: String::new(),
            styles: vec![theme::body()],
            lists: Vec::new(),
            quotes: 0,
            pending_marker: None,
            code: None,
            table: None,
        }
    }

    fn finish(mut self) -> Rendered {
        self.flush_block();
        // A trailing blank line is the block separator of the last block; the
        // transcript adds its own spacing between messages.
        while self.out.last().is_some_and(|line| line.width() == 0) {
            self.out.pop();
            // Links on a line that no longer exists must go too, or a click
            // resolves against a row belonging to the next message.
            let remaining = self.out.len();
            self.links.retain(|link| link.line < remaining);
        }
        Rendered {
            lines: self.out,
            links: self.links,
            urls: self.urls,
        }
    }

    fn style(&self) -> Style {
        *self.styles.last().expect("style stack is never empty")
    }

    fn push_style(&mut self, apply: impl Fn(Style) -> Style) {
        let next = apply(self.style());
        self.styles.push(next);
    }

    fn pop_style(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }

    fn text(&mut self, text: &str) {
        if let Some(code) = self.code.as_mut() {
            for (index, part) in text.split('\n').enumerate() {
                if index == 0 {
                    match code.lines.last_mut() {
                        Some(last) => last.push_str(part),
                        None => code.lines.push(part.to_string()),
                    }
                } else {
                    code.lines.push(part.to_string());
                }
            }
            return;
        }
        if let Some(table) = self.table.as_mut() {
            table.cell.push_str(text);
            return;
        }
        // A newline inside a paragraph is a soft break: it is whitespace, not
        // a line the author asked for.
        self.pending.push_str(&text.replace('\n', " "));
    }

    /// Turns buffered text into segments, recognising bare URLs on the way.
    fn flush_text(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.pending);
        let style = self.style();

        // Already inside a link: its text is not scanned again, or a URL used
        // as a link's own label would nest.
        if self.link.is_some() {
            let link = self.link;
            self.segs.push(Seg { text, style, link });
            return;
        }

        // Agents paste bare URLs constantly, and a link is only clickable
        // because the renderer recorded where it landed — so an unrecognised
        // URL is not merely unstyled, it is unopenable. Done here rather than
        // by rewriting the source: this path never sees code spans or fenced
        // blocks, so there is nothing to accidentally corrupt.
        let mut rest = text.as_str();
        while let Some((before, url, after)) = split_bare_url(rest) {
            if !before.is_empty() {
                self.segs.push(Seg {
                    text: before.to_string(),
                    style,
                    link: None,
                });
            }
            self.urls.push(url.to_string());
            self.segs.push(Seg {
                text: url.to_string(),
                style: theme::link(),
                link: Some(self.urls.len() - 1),
            });
            rest = after;
        }
        if !rest.is_empty() {
            self.segs.push(Seg {
                text: rest.to_string(),
                style,
                link: None,
            });
        }
    }

    fn event(&mut self, event: Event<'_>) {
        // Anything that is not more text ends the buffered run, and does so
        // while the style that produced it is still current.
        if !matches!(event, Event::Text(_) | Event::SoftBreak) {
            self.flush_text();
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(text) => {
                let style = theme::inline_code();
                let link = self.link;
                self.segs.push(Seg {
                    text: format!(" {text} "),
                    style,
                    link,
                });
            }
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => {
                self.flush_inline(String::new(), String::new());
            }
            Event::Rule => {
                self.flush_block();
                self.out.push(Line::from(Span::styled(
                    "─".repeat(self.width),
                    theme::rule(),
                )));
                self.out.push(Line::from(""));
            }
            Event::TaskListMarker(done) => {
                let style = self.style();
                self.segs.push(Seg {
                    text: if done { "[x] ".into() } else { "[ ] ".into() },
                    style,
                    link: None,
                });
            }
            // Raw HTML in a chat message is almost always an accident. Showing
            // it verbatim is more honest than silently dropping content.
            Event::Html(text) | Event::InlineHtml(text) => self.text(&text),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => self.push_style(move |base| heading_style(base, level)),
            Tag::BlockQuote(_) => {
                self.flush_block();
                self.quotes += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush_block();
                let language = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some(CodeBlock {
                    language,
                    lines: Vec::new(),
                });
            }
            Tag::List(first) => {
                self.flush_block();
                self.lists.push(first);
            }
            Tag::Item => {
                let marker = match self.lists.last_mut() {
                    Some(Some(number)) => {
                        let marker = format!("{number}. ");
                        *number += 1;
                        marker
                    }
                    _ => "• ".to_string(),
                };
                self.pending_marker = Some(marker);
            }
            Tag::Emphasis => self.push_style(|base| base.add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(|base| base.add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self.push_style(|base| base.add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.urls.push(dest_url.to_string());
                self.link = Some(self.urls.len() - 1);
                self.push_style(|_| theme::link());
            }
            Tag::Image { dest_url, .. } => {
                // A terminal cannot show the image, so the honest rendering is
                // a labelled handle that opens it somewhere that can.
                self.urls.push(dest_url.to_string());
                self.link = Some(self.urls.len() - 1);
                let link = self.link;
                self.segs.push(Seg {
                    text: "🖼 ".to_string(),
                    style: theme::link(),
                    link,
                });
                self.push_style(|_| theme::link());
            }
            Tag::Table(_) => {
                self.flush_block();
                self.table = Some(Table {
                    header: Vec::new(),
                    rows: Vec::new(),
                    row: Vec::new(),
                    cell: String::new(),
                    in_header: false,
                });
            }
            Tag::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.in_header = true;
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) => {
                let (first, rest) = self.prefixes();
                self.flush_inline(first, rest);
                if matches!(tag, TagEnd::Heading(_)) {
                    self.pop_style();
                }
                self.out.push(Line::from(""));
            }
            TagEnd::BlockQuote(_) => {
                self.quotes = self.quotes.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                if let Some(code) = self.code.take() {
                    self.emit_code_block(code);
                }
            }
            TagEnd::List(_) => {
                self.lists.pop();
                // Blank line after the list, not after every item — items in
                // one list belong together.
                self.out.push(Line::from(""));
            }
            TagEnd::Item => {
                let (first, rest) = self.prefixes();
                self.flush_inline(first, rest);
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_style(),
            TagEnd::Link | TagEnd::Image => {
                self.link = None;
                self.pop_style();
            }
            TagEnd::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    let cell = std::mem::take(&mut table.cell);
                    table.row.push(cell.trim().to_string());
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.header = std::mem::take(&mut table.row);
                    table.in_header = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.emit_table(table);
                }
            }
            _ => {}
        }
    }

    /// The first-line and continuation prefixes for the current block, given
    /// how deep it sits in quotes and lists.
    ///
    /// They differ so a wrapped list item hangs under its own text rather than
    /// under its bullet — the thing that makes a nested list readable at all.
    fn prefixes(&mut self) -> (String, String) {
        let quote: String = "▏ ".repeat(self.quotes);
        let depth = self.lists.len().saturating_sub(1);
        let indent = " ".repeat(depth * LIST_INDENT);
        match self.pending_marker.take() {
            Some(marker) => {
                let hanging = " ".repeat(marker.width());
                (
                    format!("{quote}{indent}{marker}"),
                    format!("{quote}{indent}{hanging}"),
                )
            }
            None => (format!("{quote}{indent}"), format!("{quote}{indent}")),
        }
    }

    /// Wraps and emits the accumulated inline run.
    fn flush_inline(&mut self, first: String, rest: String) {
        self.flush_text();
        if self.segs.is_empty() {
            return;
        }
        let segs = std::mem::take(&mut self.segs);
        let budget = self.width.saturating_sub(first.width().max(rest.width()));
        for (index, line) in wrap_segments(&segs, budget.max(1)).into_iter().enumerate() {
            let prefix = if index == 0 { &first } else { &rest };
            let mut spans = Vec::new();
            // The prefix shifts every column on the line, so link positions
            // are measured from after it.
            let mut column = prefix.width();
            if !prefix.is_empty() {
                spans.push(Span::styled(prefix.clone(), theme::faint()));
            }
            let row = self.out.len();
            for seg in line {
                let seg_width = seg.text.width();
                if let Some(url) = seg.link {
                    // Merge with the run before it when they are the same link
                    // and touch, so `[some text](url)` is one target rather
                    // than one per styled fragment.
                    match self.links.last_mut() {
                        Some(last) if last.line == row && last.url == url && last.end == column => {
                            last.end = column + seg_width;
                        }
                        _ => self.links.push(LinkSpan {
                            line: row,
                            start: column,
                            end: column + seg_width,
                            url,
                        }),
                    }
                }
                column += seg_width;
                spans.push(Span::styled(seg.text, seg.style));
            }
            self.out.push(Line::from(spans));
        }
    }

    /// Emits whatever inline run is open, without a trailing blank line.
    fn flush_block(&mut self) {
        self.flush_text();
        if self.segs.is_empty() {
            return;
        }
        let (first, rest) = self.prefixes();
        self.flush_inline(first, rest);
    }

    /// A fenced block, framed and labelled with its language.
    ///
    /// Code lines hard-wrap on graphemes rather than on words: a broken
    /// identifier is recoverable, a silently dropped line is not.
    fn emit_code_block(&mut self, code: CodeBlock) {
        let mut lines = code.lines;
        while lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.pop();
        }
        if lines.is_empty() {
            return;
        }

        let quote: String = "▏ ".repeat(self.quotes);
        let available = self.width.saturating_sub(quote.width()).max(8);
        let inner = available.saturating_sub(4).max(4);
        let wrapped: Vec<String> = lines
            .iter()
            .flat_map(|line| hard_wrap(line, inner))
            .collect();
        let body_width = wrapped
            .iter()
            .map(|line| line.width())
            .max()
            .unwrap_or(0)
            .max(code.language.width() + 2)
            .min(inner);
        let box_width = body_width + 4;

        // The language label is content, not chrome: it tells the reader what
        // they are looking at, so it is styled at body weight while the frame
        // around it stays quiet.
        let mut top = vec![
            Span::styled(quote.clone(), theme::faint()),
            Span::styled("╭─", theme::code_frame()),
        ];
        let mut consumed = 2usize;
        if !code.language.is_empty() {
            let label = format!(" {} ", code.language);
            consumed += label.width();
            top.push(Span::styled(label, theme::code_label()));
        }
        top.push(Span::styled(
            format!("{}╮", "─".repeat((box_width - 1).saturating_sub(consumed))),
            theme::code_frame(),
        ));
        self.out.push(Line::from(top));
        for line in wrapped {
            let pad = body_width.saturating_sub(line.width());
            self.out.push(Line::from(vec![
                Span::styled(quote.clone(), theme::faint()),
                Span::styled("│ ", theme::code_frame()),
                Span::styled(line, theme::code()),
                Span::styled(" ".repeat(pad), theme::code()),
                Span::styled(" │", theme::code_frame()),
            ]));
        }
        self.out.push(Line::from(vec![
            Span::styled(quote, theme::faint()),
            Span::styled(
                format!("╰{}╯", "─".repeat(box_width - 2)),
                theme::code_frame(),
            ),
        ]));
        self.out.push(Line::from(""));
    }

    /// A GFM table as aligned columns under a ruled header.
    ///
    /// Column widths are content-derived, then squeezed proportionally when
    /// the total exceeds the pane — a table that overflows silently is worse
    /// than one with truncated cells, because the overflow is invisible.
    fn emit_table(&mut self, table: Table) {
        let columns = table
            .header
            .len()
            .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
        if columns == 0 {
            return;
        }
        let mut widths = vec![0usize; columns];
        for (index, cell) in table.header.iter().enumerate() {
            widths[index] = widths[index].max(cell.width());
        }
        for row in &table.rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(cell.width());
            }
        }

        let gap = 2usize;
        let total: usize = widths.iter().sum::<usize>() + gap * (columns - 1);
        if total > self.width {
            let budget = self.width.saturating_sub(gap * (columns - 1));
            let sum: usize = widths.iter().sum();
            for width in widths.iter_mut() {
                *width = (*width * budget / sum.max(1)).max(MIN_COLUMN);
            }
        }

        let render_row = |cells: &[String], widths: &[usize], style: Style| {
            let mut spans = Vec::new();
            for (index, width) in widths.iter().enumerate() {
                let cell = cells.get(index).cloned().unwrap_or_default();
                let cell = truncate(&cell, *width);
                let pad = width.saturating_sub(cell.width());
                spans.push(Span::styled(cell, style));
                spans.push(Span::raw(" ".repeat(pad)));
                if index + 1 < widths.len() {
                    spans.push(Span::raw(" ".repeat(gap)));
                }
            }
            Line::from(spans)
        };

        if !table.header.is_empty() {
            self.out.push(render_row(
                &table.header,
                &widths,
                theme::body().add_modifier(Modifier::BOLD),
            ));
            let rule: Vec<String> = widths.iter().map(|width| "─".repeat(*width)).collect();
            self.out.push(render_row(&rule, &widths, theme::rule()));
        }
        for row in &table.rows {
            self.out.push(render_row(row, &widths, theme::body()));
        }
        self.out.push(Line::from(""));
    }
}

/// Splits off the first bare URL: `(before, url, after)`.
///
/// Trailing punctuation is excluded, because a sentence ending in a link
/// almost always puts the full stop outside it — `see https://example.com.`
/// means the site, not a path ending in a dot. Closing brackets are trimmed
/// only when unmatched, so a URL genuinely containing one survives.
fn split_bare_url(text: &str) -> Option<(&str, &str, &str)> {
    let start = ["https://", "http://"]
        .iter()
        .filter_map(|scheme| text.find(scheme))
        .min()?;
    let tail = &text[start..];
    let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
    let mut url = &tail[..end];

    loop {
        let last = url.chars().last()?;
        let trim = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' => true,
            ')' => url.matches('(').count() < url.matches(')').count(),
            ']' => url.matches('[').count() < url.matches(']').count(),
            '}' => url.matches('{').count() < url.matches('}').count(),
            _ => false,
        };
        if !trim {
            break;
        }
        url = &url[..url.len() - last.len_utf8()];
    }

    // A scheme with nothing after it is not a URL.
    if url.ends_with("://") {
        return None;
    }
    Some((&text[..start], url, &text[start + url.len()..]))
}

fn heading_style(base: Style, level: HeadingLevel) -> Style {
    // Only two ranks. A terminal has no type scale, so six heading levels
    // would be six shades of the same thing; two is what the eye can use.
    match level {
        HeadingLevel::H1 | HeadingLevel::H2 => base.fg(theme::CYAN).add_modifier(Modifier::BOLD),
        _ => base.fg(theme::TEXT).add_modifier(Modifier::BOLD),
    }
}

/// Wraps styled segments to `width`, preserving each run's style across breaks.
fn wrap_segments(segs: &[Seg], width: usize) -> Vec<Vec<Seg>> {
    let mut lines: Vec<Vec<Seg>> = Vec::new();
    let mut current: Vec<Seg> = Vec::new();
    let mut used = 0usize;

    for seg in segs {
        for word in seg.text.split_inclusive(char::is_whitespace) {
            let word_width = word.width();
            if used + word_width > width && used > 0 {
                trim_end(&mut current);
                lines.push(std::mem::take(&mut current));
                used = 0;
                // A break consumes the space that caused it.
                if word.trim().is_empty() {
                    continue;
                }
            }
            // A word wider than the pane still has to land somewhere; break it
            // on grapheme clusters rather than dropping it or overflowing.
            if word_width > width {
                for cluster in word.graphemes(true) {
                    let cluster_width = cluster.width();
                    if used + cluster_width > width && used > 0 {
                        lines.push(std::mem::take(&mut current));
                        used = 0;
                    }
                    push(&mut current, cluster, seg.style, seg.link);
                    used += cluster_width;
                }
                continue;
            }
            push(&mut current, word, seg.style, seg.link);
            used += word_width;
        }
    }
    trim_end(&mut current);
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

fn push(line: &mut Vec<Seg>, text: &str, style: Style, link: Option<usize>) {
    match line.last_mut() {
        Some(last) if last.style == style && last.link == link => last.text.push_str(text),
        _ => line.push(Seg {
            text: text.to_string(),
            style,
            link,
        }),
    }
}

fn trim_end(line: &mut Vec<Seg>) {
    while let Some(last) = line.last_mut() {
        let trimmed = last.text.trim_end().to_string();
        if trimmed.is_empty() {
            line.pop();
        } else {
            last.text = trimmed;
            break;
        }
    }
}

/// Hard-wraps a code line on grapheme clusters.
fn hard_wrap(line: &str, width: usize) -> Vec<String> {
    if line.width() <= width {
        return vec![line.to_string()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut used = 0usize;
    for cluster in line.graphemes(true) {
        let cluster_width = cluster.width();
        if used + cluster_width > width && used > 0 {
            out.push(std::mem::take(&mut current));
            used = 0;
        }
        current.push_str(cluster);
        used += cluster_width;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width <= 1 {
        return "…".repeat(width);
    }
    let mut out = String::new();
    let mut used = 0usize;
    for cluster in text.graphemes(true) {
        let cluster_width = cluster.width();
        if used + cluster_width > width - 1 {
            break;
        }
        out.push_str(cluster);
        used += cluster_width;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rendered text, with styling dropped — enough to assert what a
    /// reader sees without pinning colors.
    fn plain(content: &str, width: usize) -> Vec<String> {
        render(content, width)
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn styles_of(content: &str, width: usize) -> Vec<Style> {
        render(content, width)
            .lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.style).collect::<Vec<_>>())
            .collect()
    }

    #[test]
    fn emphasis_markers_are_consumed_not_shown() {
        // The whole point: an agent writing **bold** should not put asterisks
        // on the reader's screen.
        assert_eq!(plain("**bold** and *thin*", 40), vec!["bold and thin"]);
        assert!(styles_of("**bold**", 40)
            .iter()
            .any(|style| style.add_modifier.contains(Modifier::BOLD)));
    }

    #[test]
    fn a_bold_run_that_wraps_stays_bold_on_both_lines() {
        // This is why wrapping happens inside the renderer rather than in
        // `Paragraph`: wrapping finished spans cannot carry a style across the
        // break.
        let lines = render("**aaaa bbbb cccc dddd**", 10).lines;
        assert!(lines.len() > 1, "expected a wrap");
        for line in &lines {
            for span in &line.spans {
                if span.content.trim().is_empty() {
                    continue;
                }
                assert!(
                    span.style.add_modifier.contains(Modifier::BOLD),
                    "{:?} lost its emphasis after the break",
                    span.content
                );
            }
        }
    }

    #[test]
    fn bullets_hang_under_their_own_text_not_under_the_marker() {
        // A wrapped list item that returns to the bullet column is unreadable
        // as a list.
        let lines = plain("- alpha beta gamma delta epsilon", 16);
        assert!(lines[0].starts_with("• alpha"), "{lines:?}");
        assert!(
            lines[1].starts_with("  "),
            "continuation must be indented past the bullet: {lines:?}"
        );
    }

    #[test]
    fn ordered_lists_number_themselves() {
        let lines = plain("1. one\n2. two", 40);
        assert_eq!(lines[0], "1. one");
        assert_eq!(lines[1], "2. two");
    }

    #[test]
    fn nested_lists_indent_by_level() {
        let lines = plain("- outer\n  - inner", 40);
        assert_eq!(lines[0], "• outer");
        assert!(lines[1].starts_with("  •"), "{lines:?}");
    }

    #[test]
    fn a_fenced_block_is_framed_and_labelled() {
        let lines = plain("```rust\nlet x = 1;\n```", 40);
        assert!(lines[0].contains("rust"), "{lines:?}");
        assert!(lines[0].starts_with('╭'));
        assert!(lines.iter().any(|line| line.contains("let x = 1;")));
        assert!(lines.iter().any(|line| line.starts_with('╰')));
    }

    #[test]
    fn a_long_code_line_is_broken_rather_than_lost() {
        // Truncating code silently loses the end of a command someone is meant
        // to run.
        let code = "cargo run --release -- --relay wss://example.com --channel abc";
        let lines = plain(&format!("```\n{code}\n```"), 30);
        let joined: String = lines
            .iter()
            .filter(|line| line.starts_with('│'))
            .map(|line| {
                line.trim_start_matches('│')
                    .trim()
                    .trim_end_matches('│')
                    .trim()
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(joined.contains("wss://example.com"), "{lines:?}");
    }

    #[test]
    fn a_code_block_never_exceeds_the_pane() {
        for width in [20usize, 32, 60] {
            for line in render("```\nabcdefghijklmnopqrstuvwxyz0123456789\n```", width).lines {
                assert!(line.width() <= width, "width {width}: {line:?}");
            }
        }
    }

    #[test]
    fn inline_code_keeps_its_text_and_gains_a_ground() {
        let lines = plain("run `buzz canvas get` now", 40);
        assert!(lines[0].contains("buzz canvas get"), "{lines:?}");
        assert!(
            styles_of("`x`", 40).iter().any(|style| style.bg.is_some()),
            "inline code should be tinted"
        );
    }

    #[test]
    fn a_table_aligns_its_columns_and_fits_the_pane() {
        let table = "| Kind | Name |\n| --- | --- |\n| 9 | message |\n| 7 | reaction |";
        for width in [24usize, 40, 72] {
            for line in render(table, width).lines {
                assert!(line.width() <= width, "width {width}: {line:?}");
            }
        }
        let lines = plain(table, 40);
        assert!(
            lines.iter().any(|line| line.contains("reaction")),
            "{lines:?}"
        );
    }

    #[test]
    fn block_quotes_are_marked_down_the_left() {
        let lines = plain("> quoted", 40);
        assert!(lines[0].starts_with('▏'), "{lines:?}");
        assert!(lines[0].contains("quoted"));
    }

    #[test]
    fn a_link_shows_its_text_not_its_url() {
        let lines = plain("see [the issue](https://example.com/very/long/path)", 40);
        assert!(lines[0].contains("the issue"), "{lines:?}");
        assert!(!lines[0].contains("example.com"), "{lines:?}");
        assert!(styles_of("[a](https://b)", 40)
            .iter()
            .any(|style| style.add_modifier.contains(Modifier::UNDERLINED)));
    }

    #[test]
    fn a_link_records_where_it_landed() {
        // A link is only clickable because the renderer says which columns it
        // occupies — there is no OSC-8 here, the app resolves the click itself.
        let rendered = render("see [the issue](https://example.com/x) now", 60);
        assert_eq!(rendered.urls, vec!["https://example.com/x".to_string()]);
        assert_eq!(rendered.links.len(), 1);
        let link = &rendered.links[0];
        assert_eq!(link.line, 0);

        // The recorded columns must cover exactly the visible link text.
        let text: String = rendered.lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        let covered: String = text
            .chars()
            .skip(link.start)
            .take(link.end - link.start)
            .collect();
        assert_eq!(covered, "the issue");
    }

    #[test]
    fn a_link_that_wraps_is_clickable_on_every_line_it_covers() {
        // A terminal has no notion of a shape spanning rows, so each fragment
        // has to be its own target or half the link stops responding.
        let rendered = render("[alpha beta gamma delta](https://example.com)", 12);
        assert!(rendered.lines.len() > 1, "expected a wrap");
        let rows: Vec<usize> = rendered.links.iter().map(|link| link.line).collect();
        assert!(rows.len() > 1, "only one row was clickable: {rows:?}");
        assert!(rendered.links.iter().all(|link| link.url == 0));
    }

    #[test]
    fn a_bare_url_is_a_link_too() {
        // Agents paste these constantly; without GFM autolinks they would be
        // plain text and unopenable.
        let rendered = render("ship it https://example.com/deploy please", 60);
        assert_eq!(rendered.urls.len(), 1, "{:?}", rendered.urls);
        assert!(!rendered.links.is_empty());
    }

    #[test]
    fn a_url_keeps_the_sentence_punctuation_out_of_itself() {
        // "see https://example.com." means the site, not a path ending in a
        // dot — and an opener handed the dot gets a 404.
        let cases = [
            ("see https://example.com.", "https://example.com"),
            ("(https://example.com)", "https://example.com"),
            ("https://example.com/a_(b)", "https://example.com/a_(b)"),
            ("ask https://example.com?", "https://example.com"),
        ];
        for (input, expected) in cases {
            let rendered = render(input, 80);
            assert_eq!(rendered.urls, vec![expected.to_string()], "{input:?}");
        }
    }

    #[test]
    fn a_bare_scheme_is_not_a_url() {
        assert!(render("https:// is a scheme", 60).urls.is_empty());
    }

    #[test]
    fn a_url_inside_code_is_left_alone() {
        // Code is content, not a link: `curl https://x` is an instruction to
        // type, and turning part of it into a click target misreads it.
        let rendered = render("run `curl https://example.com` now", 60);
        assert!(rendered.urls.is_empty(), "{:?}", rendered.urls);
        let fenced = render("```\ncurl https://example.com\n```", 60);
        assert!(fenced.urls.is_empty(), "{:?}", fenced.urls);
    }

    #[test]
    fn a_url_used_as_its_own_link_label_is_not_doubled() {
        let rendered = render("[https://example.com](https://example.com)", 60);
        assert_eq!(rendered.urls.len(), 1, "{:?}", rendered.urls);
    }

    #[test]
    fn two_urls_in_one_line_are_separate_targets() {
        let rendered = render("https://a.example.com and https://b.example.com", 80);
        assert_eq!(rendered.urls.len(), 2);
        assert_eq!(rendered.links.len(), 2);
        let (first, second) = (&rendered.links[0], &rendered.links[1]);
        assert!(first.end <= second.start, "targets must not overlap");
    }

    #[test]
    fn an_image_becomes_a_handle_that_opens_it() {
        // A terminal cannot draw the picture, so the honest rendering is a
        // labelled target pointing at something that can.
        let rendered = render("![a diagram](https://example.com/d.png)", 60);
        assert_eq!(rendered.urls, vec!["https://example.com/d.png".to_string()]);
        let text: String = rendered.lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("🖼"), "{text:?}");
        assert!(text.contains("a diagram"), "{text:?}");
        assert!(!rendered.links.is_empty(), "the handle must be clickable");
    }

    #[test]
    fn links_on_trimmed_trailing_lines_are_dropped() {
        // Trailing blanks are removed after rendering; a target left pointing
        // at a row that no longer exists would resolve against the next
        // message.
        let rendered = render("[x](https://example.com)\n\n\n", 40);
        let rows = rendered.lines.len();
        assert!(rendered.links.iter().all(|link| link.line < rows));
    }

    #[test]
    fn wide_glyphs_wrap_on_width_not_on_character_count() {
        // Eight CJK characters are sixteen columns wide.
        for line in render("一二三四五六七八", 8).lines {
            assert!(line.width() <= 8, "{line:?}");
        }
    }

    #[test]
    fn a_word_longer_than_the_pane_is_broken_rather_than_dropped() {
        let long = "x".repeat(30);
        let joined = plain(&long, 10).join("");
        assert_eq!(joined, long);
    }

    #[test]
    fn plain_text_survives_untouched() {
        // The common case is not markdown at all.
        assert_eq!(plain("just a message", 40), vec!["just a message"]);
    }

    #[test]
    fn no_output_ends_with_padding_blank_lines() {
        // The transcript adds its own spacing between messages; trailing
        // blanks here would double it.
        let lines = render("# Heading\n\nbody\n\n", 40).lines;
        assert!(lines.last().is_some_and(|line| line.width() > 0));
    }

    #[test]
    fn an_empty_message_renders_nothing() {
        assert!(render("", 40).lines.is_empty());
    }
}
