//! Rebuilds reading lines from pdf-inspector's positioned text items.
//!
//! Invoices are mostly tables and label/value pairs, so a line keeps its spans
//! with their horizontal positions: a value is looked up to the right of its
//! label on the same line, or in the same column on the lines below.

use pdf_inspector::TextItem;
use serde::Serialize;

/// A positioned run of text: the part of pdf-inspector's [`TextItem`] the
/// layout needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// 1-indexed page number.
    pub page: u32,
    pub text: String,
    pub x: f32,
    /// Baseline in PDF points from the page's bottom edge.
    pub y: f32,
    pub width: f32,
    /// Font size.
    pub height: f32,
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
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Span {
    pub text: String,
    pub x: f32,
    pub width: f32,
}

impl Span {
    pub fn right(&self) -> f32 {
        self.x + self.width
    }
}

/// One visual line of a page, top to bottom.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Line {
    /// 1-indexed page number.
    pub page: u32,
    /// Baseline in PDF points from the page's bottom edge.
    pub y: f32,
    /// Font size of the tallest run on the line.
    pub size: f32,
    pub spans: Vec<Span>,
}

impl Line {
    /// The spans joined with single spaces.
    pub fn text(&self) -> String {
        self.spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
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
            let tolerance = (first.height.max(item.height) * 0.35).max(1.5);
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
        let gap_limit = item.height.max(1.0) * 0.6;

        match spans.last_mut() {
            Some(last) if item.x - last.right() <= gap_limit => {
                // A visible gap inside a span is a word break.
                if item.x - last.right() > item.height.max(1.0) * 0.15 && !last.text.ends_with(' ') {
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

    Line {
        page: items[0].page,
        y: items.iter().map(|item| item.y).sum::<f32>() / items.len() as f32,
        size: items.iter().map(|item| item.height).fold(0.0, f32::max),
        spans,
    }
}

fn is_horizontal(rotation: f32) -> bool {
    !(5.0..=355.0).contains(&rotation)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A positioned text item for tests; `width` is estimated from the length.
    pub(crate) fn item(page: u32, x: f32, y: f32, text: &str) -> Item {
        Item {
            page,
            text: text.to_owned(),
            x,
            y,
            width: text.chars().count() as f32 * 5.0,
            height: 10.0,
            rotation: 0.0,
        }
    }

    #[test]
    fn groups_items_on_one_baseline_into_a_line() {
        let lines = lines(&[
            item(1, 300.0, 700.4, "4.690,00 €"),
            item(1, 50.0, 700.0, "Nettosumme:"),
            item(1, 50.0, 680.0, "MwSt. 19,00 %:"),
        ]);

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text(), "Nettosumme: 4.690,00 €");
        assert_eq!(lines[0].spans.len(), 2);
        assert_eq!(lines[1].text(), "MwSt. 19,00 %:");
    }

    #[test]
    fn merges_adjacent_items_into_one_span() {
        let lines = lines(&[item(1, 50.0, 700.0, "Rechnungs-"), item(1, 100.0, 700.0, "Nr.")]);

        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[0].spans[0].text, "Rechnungs-Nr.");
    }

    #[test]
    fn orders_pages_and_lines_top_to_bottom() {
        let lines = lines(&[
            item(2, 50.0, 700.0, "zweite Seite"),
            item(1, 50.0, 100.0, "unten"),
            item(1, 50.0, 700.0, "oben"),
        ]);

        let texts: Vec<_> = lines.iter().map(|line| (line.page, line.text())).collect();
        assert_eq!(
            texts,
            [
                (1, "oben".to_owned()),
                (1, "unten".to_owned()),
                (2, "zweite Seite".to_owned())
            ]
        );
    }
}
