//! Rebuilds reading lines from pdf-inspector's positioned text items.
//!
//! Invoices are mostly tables and label/value pairs, so a line keeps its spans
//! with their horizontal positions: a value is looked up to the right of its
//! label on the same line, or in the same column on the lines below.

use std::ops::RangeInclusive;

use pdf_inspector::TextItem;

/// Items whose baselines differ by at most this fraction of the larger font
/// size share a line.
const BASELINE_TOLERANCE: f32 = 0.35;

/// The least distance, in PDF points, two baselines may differ and still
/// share a line, whatever the font size.
const MIN_BASELINE_TOLERANCE: f32 = 1.5;

/// Items closer than this fraction of the font size merge into one span.
const SPAN_GAP: f32 = 0.6;

/// A gap inside a span wider than this fraction of the font size is a word
/// break and gets a space.
const WORD_GAP: f32 = 0.15;

/// Rotations, in degrees, of text that is not horizontal: vertical, upside
/// down or in between. Such items are left out of the lines.
const ROTATED: RangeInclusive<f32> = 5.0..=355.0;

/// A positioned run of text: the part of pdf-inspector's [`TextItem`] the
/// layout needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// 1-indexed page number.
    pub page: u32,
    /// The run's text as extracted.
    pub text: String,
    /// Left edge in PDF points from the page's left edge.
    pub x: f32,
    /// Baseline in PDF points from the page's bottom edge.
    pub y: f32,
    /// Width in PDF points.
    pub width: f32,
    /// Font size.
    pub height: f32,
    /// Rotation in degrees, counter-clockwise.
    pub rotation: f32,
}

impl From<&TextItem> for Item {
    fn from(item: &TextItem) -> Self {
        Self {
            page: item.page,
            text: item.text.clone(),
            x: item.x,
            y: item.y,
            width: item.width,
            height: item.height,
            rotation: item.rotation,
        }
    }
}

/// A run of text on a line, in PDF points from the page's left edge.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    /// The run's text, trimmed.
    pub text: String,
    /// Left edge.
    pub x: f32,
    /// Width.
    pub width: f32,
}

impl Span {
    /// The right edge.
    #[must_use]
    pub fn right(&self) -> f32 {
        self.x + self.width
    }
}

/// One visual line of a page, top to bottom.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    /// 1-indexed page number.
    pub page: u32,
    /// Baseline in PDF points from the page's bottom edge.
    pub y: f32,
    /// The runs, left to right. Private so that `text` stays their join.
    spans: Vec<Span>,
    /// The spans joined with single spaces, built once: every reader of a
    /// line asks for its text.
    text: String,
}

impl Line {
    /// A line of `spans` on `page` at baseline `y`.
    #[must_use]
    pub fn new(page: u32, y: f32, spans: Vec<Span>) -> Self {
        let text = spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");

        Self { page, y, spans, text }
    }

    /// The runs, left to right.
    #[must_use]
    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    /// The spans joined with single spaces.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Groups horizontal text items into lines. Items whose baselines lie within a
/// fraction of the font size of each other form one line; on a line, items
/// closer than a space width merge into one span.
pub fn lines(items: &[Item]) -> Vec<Line> {
    let mut items: Vec<&Item> = items
        .iter()
        .filter(|item| !item.text.trim().is_empty() && is_horizontal(item.rotation))
        .collect();

    // Top to bottom, then left to right; pages in order.
    items.sort_by(|a, b| a.page.cmp(&b.page).then(b.y.total_cmp(&a.y)).then(a.x.total_cmp(&b.x)));

    let mut lines: Vec<Line> = Vec::new();
    let mut current: Vec<&Item> = Vec::new();

    for item in items {
        let starts_new_line = current.first().is_some_and(|first| {
            let tolerance = (first.height.max(item.height) * BASELINE_TOLERANCE).max(MIN_BASELINE_TOLERANCE);
            first.page != item.page || (first.y - item.y).abs() > tolerance
        });

        if starts_new_line {
            lines.push(build_line(&current));
            current.clear();
        }

        current.push(item);
    }

    if !current.is_empty() {
        lines.push(build_line(&current));
    }

    lines
}

/// The text of each page, line by line.
pub fn page_texts(lines: &[Line], page_count: u32) -> Vec<String> {
    (1..=page_count)
        .map(|page| {
            lines
                .iter()
                .filter(|line| line.page == page)
                .map(Line::text)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect()
}

fn build_line(items: &[&Item]) -> Line {
    let mut sorted = items.to_vec();
    sorted.sort_by(|a, b| a.x.total_cmp(&b.x));

    let mut spans: Vec<Span> = Vec::new();

    for item in sorted {
        let text = item.text.trim();
        let gap_limit = item.height.max(1.0) * SPAN_GAP;

        match spans.last_mut() {
            Some(last) if item.x - last.right() <= gap_limit => {
                // A visible gap inside a span is a word break.
                if item.x - last.right() > item.height.max(1.0) * WORD_GAP && !last.text.ends_with(' ') {
                    last.text.push(' ');
                }
                last.text.push_str(text);
                last.width = (item.x + item.width - last.x).max(last.width);
            }
            _ => spans.push(Span {
                text: text.to_owned(),
                x: item.x,
                width: item.width,
            }),
        }
    }

    Line::new(
        items[0].page,
        items.iter().map(|item| item.y).sum::<f32>() / items.len() as f32,
        spans,
    )
}

fn is_horizontal(rotation: f32) -> bool {
    !ROTATED.contains(&rotation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::item;

    #[test]
    fn groups_items_on_one_baseline_into_a_line() {
        let lines = lines(&[
            item(1, 300.0, 700.4, "4.690,00 €"),
            item(1, 50.0, 700.0, "Nettosumme:"),
            item(1, 50.0, 680.0, "MwSt. 19,00 %:"),
        ]);

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text(), "Nettosumme: 4.690,00 €");
        assert_eq!(lines[0].spans().len(), 2);
        assert_eq!(lines[1].text(), "MwSt. 19,00 %:");
    }

    #[test]
    fn merges_adjacent_items_into_one_span() {
        let lines = lines(&[item(1, 50.0, 700.0, "Rechnungs-"), item(1, 100.0, 700.0, "Nr.")]);

        assert_eq!(lines[0].spans().len(), 1);
        assert_eq!(lines[0].spans()[0].text, "Rechnungs-Nr.");
    }

    #[test]
    fn orders_pages_and_lines_top_to_bottom() {
        let lines = lines(&[
            item(2, 50.0, 700.0, "zweite Seite"),
            item(1, 50.0, 100.0, "unten"),
            item(1, 50.0, 700.0, "oben"),
        ]);

        let texts: Vec<_> = lines.iter().map(|line| (line.page, line.text())).collect();
        assert_eq!(texts, [(1, "oben"), (1, "unten"), (2, "zweite Seite")]);
    }
}
