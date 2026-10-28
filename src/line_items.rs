//! Line items of German quotes and invoices, read from reconstructed lines.
//!
//! A position starts with a line whose first span is a position number alone
//! (`1.2`, `01.11.01.01`, `10073`), always in the same column. Its quantity
//! sits in a span of its own, as a number and a unit (`4,000 Wochen`, `1 psch`)
//! or as a lump-sum word (`pauschal`), on the first line or the last line of
//! the position's text, followed by the unit price and the total. Header and
//! footer lines repeated on several pages are skipped, so a position can run
//! across a page break.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;
use utoipa::ToSchema;

use crate::layout::{Line, Span};

/// One position of a quote or invoice.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct LineItem {
    /// The position number as printed, e.g. `1.2` or `10073`.
    pub position: String,
    /// The position's text, its lines joined with spaces.
    pub description: String,
    /// Decimal string with a dot, e.g. `82.45`; `1` for a lump sum.
    pub quantity: Option<String>,
    /// The unit as printed, e.g. `m²`, `Stk` or `pauschal`.
    pub unit: Option<String>,
    /// Decimal strings with a dot and two fraction digits.
    pub unit_price: Option<String>,
    pub total: Option<String>,
    /// An optional, alternative or contingency position, which is not part of
    /// the document's total.
    pub optional: bool,
    /// The heading of the title or group the position belongs to.
    pub section: Option<String>,
    /// 1-indexed page number of the position's first line.
    pub page: u32,
}

macro_rules! regex {
    ($name:ident, $pattern:literal) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pattern).expect("valid regex"));
    };
}

regex!(POSITION, r"^(?:\d{1,3}(?:\.\d{1,3}){1,5}|\d{1,6})\.?$");
regex!(
    QUANTITY,
    r"^(-?\d{1,3}(?:\.\d{3})+(?:,\d+)?|-?\d+(?:,\d+)?)\s*([A-Za-zÄÖÜäöüß][A-Za-zÄÖÜäöüß²³./]{0,11})$"
);
regex!(LUMP_SUM, r"(?i)^(?:pauschal|pausch\.?|psch\.?|pschl\.?)$");
regex!(AMOUNT, r"^(-?\d{1,3}(?:\.\d{3})*,\d{2}|-?\d+,\d{2})\s*(?:€|EUR)?$");
regex!(
    OPTIONAL_MARKER,
    r"(?i)^(?:bedarf|bedarfsposition|eventual|eventualposition|alternativ|alternativposition|optional|alt\.|nur\s+ep)$"
);
regex!(
    OPTIONAL_SECTION,
    r"(?i)eventual|optional|alternativ|nicht\s+in\s+der\s+angebotssumme|nicht\s+im\s+(?:basis|gesamt|angebots)preis"
);
regex!(NOTE, r"(?i)^(?:hinweis|achtung|anmerkung|bitte\s+beachten|wichtig)\b");
regex!(
    TOTAL_LINE,
    r"(?i)^(?:übertrag|titelsumme|zwischensumme|gesamt|summe|nachlass|rabatt|skonto|netto|brutto)|mehrwertsteuer|umsatzsteuer|\bmwst\b"
);

/// How far, in PDF points, a position number may sit from the column of the
/// first one.
const COLUMN_TOLERANCE: f32 = 6.0;
/// Lines at the top and bottom of a page that may be a repeated header or
/// footer.
const HEADER_LINES: usize = 6;
const FOOTER_LINES: usize = 4;

/// The positions of the document; empty when it has fewer than two, which
/// leaves letters and receipts without a misread table.
pub fn extract(lines: &[Line]) -> Vec<LineItem> {
    let repeated = repeated_lines(lines);
    let lines: Vec<&Line> = lines
        .iter()
        .filter(|line| !line.spans.is_empty() && !repeated.contains(&normalized(line)))
        .collect();

    let mut items = Vec::new();
    let mut open: Option<Open> = None;
    let mut column: Option<f32> = None;
    let mut heading = Heading::default();

    for line in lines {
        let starts_position = is_position(&line.spans[0])
            && column.is_none_or(|x| (line.spans[0].x - x).abs() <= COLUMN_TOLERANCE)
            && line.spans.len() > 1;

        if starts_position {
            if let Some(item) = open.take().and_then(Open::finish) {
                column.get_or_insert(item.column);
                items.push(item.item);
            }
            open = Some(Open::start(line, heading.section.clone(), heading.optional));
            heading.used = true;
            continue;
        }

        let text = line.text();
        let is_total = TOTAL_LINE.is_match(text.trim()) && line.spans.iter().any(|span| is_amount(&span.text));

        if let Some(current) = open.as_mut() {
            if !is_total && current.continues_with(line) {
                current.lines.push(line.clone());
                continue;
            }
            if let Some(item) = open.take().and_then(Open::finish) {
                column.get_or_insert(item.column);
                items.push(item.item);
            }
        }

        if !is_total {
            heading.read(&text);
        }
    }

    if let Some(item) = open.and_then(Open::finish) {
        items.push(item.item);
    }

    if items.len() < 2 {
        return Vec::new();
    }

    items
}

/// The title or group that the following positions belong to, read from the
/// text between positions.
#[derive(Default)]
struct Heading {
    section: Option<String>,
    kind: HeadingKind,
    optional: bool,
    /// Whether a position followed the current heading; the explanatory text
    /// under a heading does not replace it until then.
    used: bool,
}

#[derive(Default, PartialEq)]
enum HeadingKind {
    #[default]
    Plain,
    /// `Titel: 2. Erdungsarbeiten`, replaced only by the next title.
    Title,
    /// A line ending with a colon.
    Labelled,
}

impl Heading {
    fn read(&mut self, text: &str) {
        let text = text.trim();
        let (kind, name) = match text.strip_prefix("Titel:") {
            Some(name) => (HeadingKind::Title, name.trim()),
            None if text.ends_with(':') => (HeadingKind::Labelled, text.trim_end_matches(':').trim()),
            None => (HeadingKind::Plain, text),
        };

        if OPTIONAL_SECTION.is_match(name) {
            self.optional = true;
        } else if kind != HeadingKind::Plain {
            self.optional = false;
        }

        let replaces = match kind {
            HeadingKind::Title => true,
            _ if self.kind == HeadingKind::Title => false,
            HeadingKind::Labelled => true,
            HeadingKind::Plain => {
                !name.starts_with('(') && (self.kind == HeadingKind::Plain || self.used || is_title(name))
            }
        };

        if replaces {
            self.section = Some(name.to_owned());
            self.kind = kind;
            self.used = false;
        }
    }
}

/// A position while its lines are collected.
struct Open {
    lines: Vec<Line>,
    section: Option<String>,
    optional: bool,
}

struct Finished {
    item: LineItem,
    column: f32,
}

impl Open {
    fn start(line: &Line, section: Option<String>, optional: bool) -> Self {
        Self {
            lines: vec![line.clone()],
            section,
            optional,
        }
    }

    /// The x of the first text after the position number: on the first line,
    /// or on the next line when the first holds only figures.
    fn description_x(&self) -> Option<f32> {
        self.lines
            .iter()
            .flat_map(|line| line.spans.iter().skip(usize::from(line == &self.lines[0])))
            .find(|span| span_kind(span) == SpanKind::Text)
            .map(|span| span.x)
    }

    /// A following line belongs to the position when it starts in the
    /// description's column; a line ending with a colon at the position
    /// number's column heads a new group instead.
    fn continues_with(&self, line: &Line) -> bool {
        let first = &line.spans[0];
        let number_x = self.lines[0].spans[0].x;

        match self.description_x() {
            Some(x) if (x - number_x).abs() <= COLUMN_TOLERANCE => {
                let text = line.text();
                !text.trim_end().ends_with(':') || NOTE.is_match(&text)
            }
            Some(x) => first.x >= x - COLUMN_TOLERANCE,
            // The text starts on the next line, possibly in the number's column.
            None => first.x >= number_x - COLUMN_TOLERANCE,
        }
    }

    fn finish(self) -> Option<Finished> {
        let first = &self.lines[0];
        let number = &first.spans[0];
        let mut quantity = None;
        let mut unit = None;
        let mut figures: Vec<String> = Vec::new();
        let mut optional = self.optional;
        let mut description: Vec<String> = Vec::new();

        for (index, line) in self.lines.iter().enumerate() {
            let mut text: Vec<&str> = Vec::new();
            let mut after_quantity = false;

            for span in line.spans.iter().skip(usize::from(index == 0)) {
                match span_kind(span) {
                    SpanKind::Quantity(value, symbol) if quantity.is_none() => {
                        quantity = Some(value);
                        unit = Some(symbol);
                        after_quantity = true;
                    }
                    SpanKind::Amount(value) if after_quantity || (index == 0 && figures.len() < 2) => {
                        figures.push(value);
                    }
                    SpanKind::Marker => optional = true,
                    SpanKind::Text => text.push(&span.text),
                    _ => {}
                }
            }

            if !text.is_empty() {
                description.push(text.join(" "));
            }
        }

        if quantity.is_none() && figures.is_empty() {
            return None;
        }

        // Two figures are the unit price and the total; one is the total,
        // unless the position is marked optional, which prices only its unit.
        let (unit_price, total) = match figures.as_slice() {
            [price, total, ..] => (Some(price.clone()), Some(total.clone())),
            [only] if optional && self.lines.iter().any(has_marker_after_amount) => (Some(only.clone()), None),
            [only] => (None, Some(only.clone())),
            [] => (None, None),
        };

        Some(Finished {
            item: LineItem {
                position: number.text.trim_end_matches('.').to_owned(),
                description: join_wrapped(&description),
                quantity,
                unit,
                unit_price,
                total,
                optional,
                section: self.section,
                page: first.page,
            },
            column: number.x,
        })
    }
}

#[derive(Debug, PartialEq)]
enum SpanKind {
    Quantity(String, String),
    Amount(String),
    Marker,
    Text,
}

fn span_kind(span: &Span) -> SpanKind {
    let text = span.text.trim();

    if LUMP_SUM.is_match(text) {
        return SpanKind::Quantity("1".to_owned(), text.to_owned());
    }
    if let Some(captures) = AMOUNT.captures(text) {
        return SpanKind::Amount(money(&captures[1]));
    }
    if let Some(captures) = QUANTITY.captures(text) {
        return SpanKind::Quantity(number(&captures[1]), captures[2].to_owned());
    }
    if OPTIONAL_MARKER.is_match(text) {
        return SpanKind::Marker;
    }

    SpanKind::Text
}

/// A short line that does not end like a sentence, such as `Erdungsanlage`.
fn is_title(text: &str) -> bool {
    text.chars().count() <= 40 && !text.ends_with(['.', ',', ';'])
}

fn is_position(span: &Span) -> bool {
    POSITION.is_match(span.text.trim())
}

fn is_amount(text: &str) -> bool {
    AMOUNT.is_match(text.trim())
}

fn has_marker_after_amount(line: &Line) -> bool {
    line.spans
        .iter()
        .skip_while(|span| !is_amount(&span.text))
        .any(|span| OPTIONAL_MARKER.is_match(span.text.trim()))
}

/// `1.234,56` as `1234.56`.
fn money(amount: &str) -> String {
    let cents: i64 = amount.replace(['.', ','], "").parse().unwrap_or_default();

    format!(
        "{}{}.{:02}",
        if cents < 0 { "-" } else { "" },
        cents.abs() / 100,
        cents.abs() % 100
    )
}

/// `1.234,500` as `1234.5`.
fn number(quantity: &str) -> String {
    let decimal = quantity.replace('.', "").replace(',', ".");

    if decimal.contains('.') {
        decimal.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        decimal
    }
}

/// Joins wrapped lines; a word hyphenated at a line end is joined again.
fn join_wrapped(lines: &[String]) -> String {
    let mut text = String::new();

    for line in lines {
        let line = line.trim();
        let hyphenated = text.ends_with('-')
            && text[..text.len() - 1].ends_with(char::is_alphabetic)
            && line.starts_with(char::is_lowercase);

        if hyphenated {
            text.pop();
        } else if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(line);
    }

    text
}

/// Lines at the top or bottom of at least two pages, compared with their
/// digits blanked: running headers, column headers, carried-over subtotals and
/// footers.
fn repeated_lines(lines: &[Line]) -> Vec<String> {
    let mut pages: HashMap<u32, Vec<&Line>> = HashMap::new();
    for line in lines.iter().filter(|line| !line.spans.is_empty()) {
        pages.entry(line.page).or_default().push(line);
    }

    let mut counts: HashMap<String, usize> = HashMap::new();
    for page in pages.values() {
        let mut seen: Vec<String> = Vec::new();
        let edges = page
            .iter()
            .take(HEADER_LINES)
            .chain(page.iter().rev().take(FOOTER_LINES))
            .filter(|line| !is_position(&line.spans[0]));

        for line in edges {
            let key = normalized(line);
            if !seen.contains(&key) {
                *counts.entry(key.clone()).or_default() += 1;
                seen.push(key);
            }
        }
    }

    counts
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .map(|(key, _)| key)
        .collect()
}

fn normalized(line: &Line) -> String {
    line.text()
        .chars()
        .map(|character| if character.is_ascii_digit() { '#' } else { character })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::lines;
    use crate::layout::tests::item;

    /// Position, quantity, unit, unit price, total and whether it is optional.
    type Row<'a> = (
        &'a str,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
        bool,
    );

    fn position(item: &LineItem) -> Row<'_> {
        (
            item.position.as_str(),
            item.quantity.as_deref(),
            item.unit.as_deref(),
            item.unit_price.as_deref(),
            item.total.as_deref(),
            item.optional,
        )
    }

    #[test]
    fn reads_positions_whose_quantity_and_prices_end_their_text() {
        let lines = lines(&[
            item(1, 40.0, 780.0, "Musterbau GmbH"),
            item(1, 300.0, 780.0, "Seite: 1"),
            item(1, 40.0, 720.0, "Titel: 1. Baustelleneinrichtung"),
            item(1, 40.0, 700.0, "1.1"),
            item(1, 105.0, 700.0, "Baustelle einrichten"),
            item(1, 105.0, 689.0, "und räumen."),
            item(1, 360.0, 689.0, "pauschal"),
            item(1, 500.0, 689.0, "1.200,00"),
            item(1, 40.0, 670.0, "1.2"),
            item(1, 105.0, 670.0, "Kran mit Laufkatzen-"),
            item(1, 105.0, 659.0, "ausleger vorhalten"),
            item(1, 330.0, 659.0, "3,000 Wochen"),
            item(1, 410.0, 659.0, "410,00"),
            item(1, 500.0, 659.0, "1.230,00"),
            item(1, 420.0, 100.0, "Übertrag:"),
            item(1, 495.0, 100.0, "2.430,00"),
            item(2, 40.0, 780.0, "Musterbau GmbH"),
            item(2, 300.0, 780.0, "Seite: 2"),
            item(2, 420.0, 750.0, "Übertrag:"),
            item(2, 495.0, 750.0, "2.430,00"),
            item(2, 40.0, 730.0, "1.3"),
            item(2, 105.0, 730.0, "Statik Kranfundament"),
            item(2, 360.0, 730.0, "pauschal"),
            item(2, 410.0, 730.0, "310,00"),
            item(2, 515.0, 730.0, "Bedarf"),
            item(2, 40.0, 700.0, "Titelsumme 1. Baustelleneinrichtung:"),
            item(2, 495.0, 700.0, "2.430,00"),
        ]);

        let items = extract(&lines);

        assert_eq!(
            items.iter().map(position).collect::<Vec<_>>(),
            [
                ("1.1", Some("1"), Some("pauschal"), None, Some("1200.00"), false),
                ("1.2", Some("3"), Some("Wochen"), Some("410.00"), Some("1230.00"), false),
                ("1.3", Some("1"), Some("pauschal"), Some("310.00"), None, true),
            ]
        );
        assert_eq!(items[1].description, "Kran mit Laufkatzenausleger vorhalten");
        assert_eq!(items[0].section.as_deref(), Some("1. Baustelleneinrichtung"));
        assert_eq!(items[2].page, 2);
    }

    #[test]
    fn reads_positions_with_the_quantity_beside_the_number() {
        let lines = lines(&[
            item(1, 58.0, 700.0, "Erdung"),
            item(1, 61.0, 680.0, "20010"),
            item(1, 109.0, 680.0, "25,00 m"),
            item(1, 177.0, 680.0, "Erdungsband aus Edelstahl verlegen,"),
            item(1, 518.0, 680.0, "437,50 €"),
            item(1, 177.0, 668.0, "in der Bodenplatte."),
            item(1, 61.0, 650.0, "20020"),
            item(1, 109.0, 650.0, "-1,00 Stk"),
            item(1, 177.0, 650.0, "Minderleistung Lichtschacht"),
            item(1, 518.0, 650.0, "-380,00 €"),
            item(1, 58.0, 630.0, "Gesamtpreis netto"),
            item(1, 504.0, 630.0, "57,50 €"),
            item(1, 58.0, 610.0, "Optionale Positionen (nicht im Basispreis enthalten):"),
            item(1, 61.0, 590.0, "20030"),
            item(1, 109.0, 590.0, "1,00 Stk"),
            item(1, 177.0, 590.0, "Baugrube vermessen"),
            item(1, 518.0, 590.0, "412,00 €"),
        ]);

        let items = extract(&lines);

        assert_eq!(
            items.iter().map(position).collect::<Vec<_>>(),
            [
                ("20010", Some("25"), Some("m"), None, Some("437.50"), false),
                ("20020", Some("-1"), Some("Stk"), None, Some("-380.00"), false),
                ("20030", Some("1"), Some("Stk"), None, Some("412.00"), true),
            ]
        );
        assert_eq!(
            items[0].description,
            "Erdungsband aus Edelstahl verlegen, in der Bodenplatte."
        );
        assert_eq!(items[0].section.as_deref(), Some("Erdung"));
    }

    #[test]
    fn reads_positions_whose_text_starts_below_the_number() {
        let lines = lines(&[
            item(1, 71.0, 700.0, "Rohbau Untergeschoss:"),
            item(1, 71.0, 680.0, "02.01.01.01"),
            item(1, 364.0, 680.0, "82,45 m²"),
            item(1, 71.0, 669.0, "Bodenplatte herstellen."),
            item(1, 71.0, 640.0, "Zusatzleistungen als Eventualposition:"),
            item(1, 71.0, 620.0, "20.05.01"),
            item(1, 383.0, 620.0, "1 psch"),
            item(1, 440.0, 620.0, "1.480,00 €"),
            item(1, 518.0, 620.0, "1.480,00 €"),
            item(1, 71.0, 609.0, "Leerrohre einlegen."),
            item(1, 71.0, 598.0, "Hinweis:"),
            item(1, 71.0, 587.0, "Nur mit Paket Haustechnik."),
            item(1, 71.0, 570.0, "20.05.02"),
            item(1, 383.0, 570.0, "1 Stück"),
            item(1, 454.0, 570.0, "58,90 €"),
            item(1, 532.0, 570.0, "58,90 €"),
            item(1, 71.0, 559.0, "Weitere Unterputzdose."),
        ]);

        let items = extract(&lines);

        assert_eq!(
            items.iter().map(position).collect::<Vec<_>>(),
            [
                ("02.01.01.01", Some("82.45"), Some("m²"), None, None, false),
                (
                    "20.05.01",
                    Some("1"),
                    Some("psch"),
                    Some("1480.00"),
                    Some("1480.00"),
                    true
                ),
                ("20.05.02", Some("1"), Some("Stück"), Some("58.90"), Some("58.90"), true),
            ]
        );
        assert_eq!(items[0].description, "Bodenplatte herstellen.");
        assert_eq!(
            items[1].description,
            "Leerrohre einlegen. Hinweis: Nur mit Paket Haustechnik."
        );
        assert_eq!(items[0].section.as_deref(), Some("Rohbau Untergeschoss"));
    }

    #[test]
    fn finds_no_positions_in_a_letter() {
        let lines = lines(&[
            item(1, 58.0, 700.0, "86150 Augsburg"),
            item(1, 58.0, 680.0, "12"),
            item(1, 300.0, 680.0, "Sehr geehrte Damen und Herren,"),
            item(1, 58.0, 660.0, "Rechnungsbetrag 119,00 €"),
        ]);

        assert!(extract(&lines).is_empty());
    }
}
