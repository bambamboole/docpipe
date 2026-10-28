//! Field candidates of German invoices and quotes, read from reconstructed
//! lines.
//!
//! Everything here is a proposal for a person to confirm: values carry the line
//! they were read from, and absent fields stay `None` rather than being guessed.

use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;
use utoipa::ToSchema;

use crate::layout::{Line, Span};
use crate::line_items::{self, LineItem};
use crate::seller;

/// What the document is, judged by its title and labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    Invoice,
    Quote,
    CreditNote,
    Other,
}

/// A standard, partial (`Abschlagsrechnung`, `Teilrechnung`) or final
/// (`Schlussrechnung`) invoice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InvoiceKind {
    Standard,
    Partial,
    Final,
}

/// A value and where it was read.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Field {
    /// The normalized value; dates as `YYYY-MM-DD`.
    pub value: String,
    /// 1-indexed page number.
    pub page: u32,
    /// The line the value was read from.
    pub line: String,
}

/// The document's totals. Without a consistent triple, only labelled amounts
/// are returned.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Amounts {
    /// Decimal strings with a dot and two fraction digits, e.g. `3250.00`.
    pub net: Option<String>,
    pub tax: Option<String>,
    pub gross: Option<String>,
    /// VAT rate in percent, e.g. `19`, when net and tax add up to gross.
    pub tax_rate: Option<u8>,
    /// Whether net + tax = gross was found on the document.
    pub consistent: bool,
}

/// Field candidates of a German invoice or quote, for a person to confirm. A
/// field that cannot be read with confidence is `null`.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Candidates {
    pub document_kind: DocumentKind,
    /// Only for invoices.
    pub invoice_kind: Option<InvoiceKind>,
    /// The invoice or quote number.
    pub number: Option<Field>,
    pub issue_date: Option<Field>,
    pub due_date: Option<Field>,
    /// The end of a quote's validity.
    pub valid_until: Option<Field>,
    pub amounts: Amounts,
    /// IBANs without spaces, checked with their ISO 13616 check digits.
    pub ibans: Vec<String>,
    /// German and Austrian VAT IDs without spaces.
    pub vat_ids: Vec<String>,
    /// The issuing company, by its name with a legal form.
    pub seller_name: Option<Field>,
    pub line_items: Vec<LineItem>,
}

pub fn extract(lines: &[Line]) -> Candidates {
    let document_kind = document_kind(lines);
    let number_label = match document_kind {
        DocumentKind::Quote => &*QUOTE_NUMBER_LABEL,
        _ => &*INVOICE_NUMBER_LABEL,
    };

    Candidates {
        document_kind,
        invoice_kind: (document_kind == DocumentKind::Invoice).then(|| invoice_kind(lines)),
        number: labelled(lines, number_label, &NUMBER),
        issue_date: labelled(lines, &ISSUE_DATE_LABEL, &DATE)
            .or_else(|| first_match(lines, &DATE))
            .map(normalize_date),
        due_date: labelled(lines, &DUE_DATE_LABEL, &DATE).map(normalize_date),
        valid_until: labelled(lines, &VALID_UNTIL_LABEL, &DATE).map(normalize_date),
        amounts: amounts(lines),
        ibans: ibans(lines),
        vat_ids: vat_ids(lines),
        seller_name: seller::extract(lines),
        line_items: line_items::extract(lines),
    }
}

macro_rules! regex {
    ($name:ident, $pattern:literal) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pattern).expect("valid regex"));
    };
}

regex!(AMOUNT, r"-?\b\d{1,3}(?:\.\d{3})*,\d{2}\b|-?\b\d+,\d{2}\b");
regex!(DATE, r"\b(\d{1,2})\.(\d{1,2})\.(\d{4}|\d{2})\b");
regex!(NUMBER, r"\b[A-Z0-9][A-Z0-9\-/\.]*\d[A-Z0-9\-/]*\b");
regex!(IBAN, r"\b[A-Z]{2}\d{2}(?: ?[A-Z0-9]{4}){2,7}(?: ?[A-Z0-9]{1,4})?\b");
regex!(VAT_ID, r"\b(?:DE ?\d{9}|ATU ?\d{8})\b");
regex!(
    INVOICE_NUMBER_LABEL,
    r"(?i)(?:\b(?:rechnungs?\s*-?\s*(?:nr|nummer|no)\.?|rechnung\s+nr\.?|belegnummer|beleg\s*-?\s*nr\.?)|(?:^|[\s,;(])nr\.)\s*:?"
);
regex!(
    QUOTE_NUMBER_LABEL,
    r"(?i)(?:\b(?:angebots?\s*-?\s*(?:nr|nummer|no)\.?|angebot\s+nr\.?|kostenvoranschlag\s+nr\.?|belegnummer|beleg\s*-?\s*nr\.?)|(?:^|[\s,;(])nr\.)\s*:?"
);
regex!(
    ISSUE_DATE_LABEL,
    r"(?i)\b(?:rechnungsdatum|angebotsdatum|belegdatum|datum|,\s*den)\s*:?"
);
regex!(
    DUE_DATE_LABEL,
    r"(?i)\b(?:fälligkeitsdatum|fällig\s+am|fällig|zahlbar\s+bis(?:\s+zum)?|zahlungsziel)\s*:?"
);
regex!(
    VALID_UNTIL_LABEL,
    r"(?i)\b(?:gültig\s+bis|bindefrist(?:\s+bis)?|angebot\s+gilt\s+bis)\s*:?"
);
regex!(
    GROSS_LABEL,
    r"(?i)\b(?:gesamtbetrag|gesamtsumme|rechnungsbetrag|bruttobetrag|endbetrag|fälliger\s+betrag|zu\s+zahlen|gesamt\s*\(brutto\)|summe\s+brutto|brutto)\b"
);
regex!(
    NET_LABEL,
    r"(?i)\b(?:nettobetrag|nettosumme|summe\s+netto|gesamt\s*\(netto\)|netto)\b"
);

/// Scores keywords; the first page, where titles sit, counts double.
fn document_kind(lines: &[Line]) -> DocumentKind {
    let mut invoice = 0;
    let mut quote = 0;
    let mut credit = 0;

    for line in lines {
        let text = line.text().to_lowercase();
        let weight = if line.page == 1 { 2 } else { 1 };

        for keyword in [
            "rechnung",
            "rechnungsnummer",
            "rechnungsdatum",
            "zahlbar",
            "fällig",
            "leistungszeitraum",
        ] {
            if text.contains(keyword) {
                invoice += weight;
            }
        }
        for keyword in [
            "angebot",
            "kostenvoranschlag",
            "angebotsnummer",
            "gültig bis",
            "bindefrist",
            "richtpreis",
        ] {
            if text.contains(keyword) {
                quote += weight;
            }
        }
        if text.contains("gutschrift") {
            credit += weight * 3;
        }
    }

    match (invoice, quote, credit) {
        (0, 0, 0) => DocumentKind::Other,
        (_, _, c) if c >= invoice && c >= quote => DocumentKind::CreditNote,
        (i, q, _) if q > i => DocumentKind::Quote,
        _ => DocumentKind::Invoice,
    }
}

fn invoice_kind(lines: &[Line]) -> InvoiceKind {
    let first_page: String = lines
        .iter()
        .filter(|line| line.page == 1)
        .map(|line| line.text().to_lowercase())
        .collect::<Vec<_>>()
        .join("\n");

    if first_page.contains("schlussrechnung") {
        InvoiceKind::Final
    } else if ["abschlagsrechnung", "teilrechnung", "abschlag"]
        .iter()
        .any(|word| first_page.contains(word))
    {
        InvoiceKind::Partial
    } else {
        InvoiceKind::Standard
    }
}

/// The first value matching `value` after a `label`: in the rest of the
/// label's span, then in the spans to its right, then in the same column on
/// the next three lines.
fn labelled(lines: &[Line], label: &Regex, value: &Regex) -> Option<Field> {
    for (index, line) in lines.iter().enumerate() {
        for (span_index, span) in line.spans.iter().enumerate() {
            let Some(found) = label.find(&span.text) else {
                continue;
            };

            let rest = &span.text[found.end()..];
            let right = line.spans[span_index + 1..].iter().map(|span| span.text.as_str());

            for candidate in std::iter::once(rest).chain(right) {
                if let Some(matched) = value.find(candidate) {
                    return Some(field(line, matched.as_str()));
                }
            }

            // Several labels can share one span, as in a table header row; the
            // label's own column is estimated from its offset in the span.
            let column = label_column(span, found.start(), found.end());

            for below in lines[index + 1..]
                .iter()
                .take(3)
                .filter(|below| below.page == line.page)
            {
                if let Some(matched) = below
                    .spans
                    .iter()
                    .filter(|candidate| overlaps(&column, candidate))
                    .find_map(|candidate| value_in_column(candidate, &column, value))
                {
                    return Some(field(below, &matched));
                }
            }
        }
    }

    None
}

fn first_match(lines: &[Line], value: &Regex) -> Option<Field> {
    lines
        .iter()
        .find_map(|line| value.find(&line.text()).map(|matched| field(line, matched.as_str())))
}

fn field(line: &Line, value: &str) -> Field {
    Field {
        value: value.trim().to_owned(),
        page: line.page,
        line: line.text(),
    }
}

/// The part of `span` between the byte offsets `start` and `end`, positioned
/// in proportion to its characters.
fn label_column(span: &Span, start: usize, end: usize) -> Span {
    let chars = span.text.chars().count().max(1) as f32;
    let before = span.text[..start].chars().count() as f32;
    let length = span.text[start..end].chars().count() as f32;

    Span {
        text: span.text[start..end].to_owned(),
        x: span.x + span.width * before / chars,
        width: span.width * length / chars,
    }
}

/// The first `value` in `candidate` that starts in or after the column; a
/// narrow candidate inside the column is searched whole.
fn value_in_column(candidate: &Span, column: &Span, value: &Regex) -> Option<String> {
    let chars = candidate.text.chars().count();
    let offset = if candidate.x >= column.x - 10.0 || candidate.width <= 0.0 {
        0
    } else {
        let ratio = ((column.x - candidate.x) / candidate.width).clamp(0.0, 1.0);
        // A small margin: proportional positions are only an estimate.
        ((ratio * chars as f32) as usize).saturating_sub(3)
    };
    let byte_offset = candidate
        .text
        .char_indices()
        .nth(offset)
        .map_or(candidate.text.len(), |(index, _)| index);

    value
        .find(&candidate.text[byte_offset..])
        .map(|matched| matched.as_str().to_owned())
}

fn overlaps(label: &Span, candidate: &Span) -> bool {
    candidate.x <= label.right() + 40.0 && candidate.right() >= label.x - 40.0
}

/// Dates become ISO `YYYY-MM-DD`; two-digit years are 20xx.
fn normalize_date(mut field: Field) -> Field {
    if let Some(captures) = DATE.captures(&field.value) {
        let day: u32 = captures[1].parse().unwrap_or(0);
        let month: u32 = captures[2].parse().unwrap_or(0);
        let year: u32 = match captures[3].parse::<u32>().unwrap_or(0) {
            year if year < 100 => 2000 + year,
            year => year,
        };

        if (1..=31).contains(&day) && (1..=12).contains(&month) {
            field.value = format!("{year:04}-{month:02}-{day:02}");
        }
    }

    field
}

/// Amounts in cents, from a German `1.234,56`.
fn cents(amount: &str) -> Option<i64> {
    amount.replace(['.', ','], "").parse().ok()
}

fn decimal(cents: i64) -> String {
    format!(
        "{}{}.{:02}",
        if cents < 0 { "-" } else { "" },
        cents.abs() / 100,
        cents.abs() % 100
    )
}

fn amounts(lines: &[Line]) -> Amounts {
    let all: Vec<i64> = lines
        .iter()
        .flat_map(|line| {
            AMOUNT
                .find_iter(&line.text())
                .filter_map(|m| cents(m.as_str()))
                .collect::<Vec<_>>()
        })
        .filter(|cents| *cents > 0)
        .collect();

    if let Some((net, tax, gross, rate)) = consistent_triple(&all) {
        return Amounts {
            net: Some(decimal(net)),
            tax: Some(decimal(tax)),
            gross: Some(decimal(gross)),
            tax_rate: Some(rate),
            consistent: true,
        };
    }

    let labelled_cents = |label: &Regex| {
        labelled(lines, label, &AMOUNT)
            .and_then(|field| cents(&field.value))
            .map(decimal)
    };

    Amounts {
        net: labelled_cents(&NET_LABEL),
        tax: None,
        // No largest-amount fallback: a wrong total is worse than none.
        gross: labelled_cents(&GROSS_LABEL),
        tax_rate: None,
        consistent: false,
    }
}

/// The largest gross amount on the document for which a net amount and a VAT
/// amount at 19 % or 7 % are also present, so that net + tax = gross.
fn consistent_triple(amounts: &[i64]) -> Option<(i64, i64, i64, u8)> {
    let mut best: Option<(i64, i64, i64, u8)> = None;

    for &gross in amounts {
        for &net in amounts.iter().filter(|net| **net < gross) {
            let tax = gross - net;

            if !amounts.contains(&tax) {
                continue;
            }

            for rate in [19, 7] {
                // Rounded per document; allow a cent of rounding difference.
                if (net * rate as i64 - tax * 100).abs() <= 100 && best.is_none_or(|(_, _, current, _)| gross > current)
                {
                    best = Some((net, tax, gross, rate));
                }
            }
        }
    }

    best
}

fn ibans(lines: &[Line]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();

    for line in lines {
        for matched in IBAN.find_iter(&line.text()) {
            let iban: String = matched.as_str().chars().filter(|c| !c.is_whitespace()).collect();

            if is_valid_iban(&iban) && !found.contains(&iban) {
                found.push(iban);
            }
        }
    }

    found
}

/// ISO 13616 check: the rearranged IBAN, letters as numbers, modulo 97 is 1.
fn is_valid_iban(iban: &str) -> bool {
    if iban.len() < 15 || iban.len() > 34 {
        return false;
    }

    let rearranged = iban[4..].chars().chain(iban[..4].chars());
    let mut remainder: u64 = 0;

    for character in rearranged {
        let digits = match character {
            '0'..='9' => character as u64 - '0' as u64,
            'A'..='Z' => character as u64 - 'A' as u64 + 10,
            _ => return false,
        };
        remainder = (remainder * if digits >= 10 { 100 } else { 10 } + digits) % 97;
    }

    remainder == 1
}

fn vat_ids(lines: &[Line]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();

    for line in lines {
        for matched in VAT_ID.find_iter(&line.text()) {
            let vat_id = matched.as_str().replace(' ', "");

            if !found.contains(&vat_id) {
                found.push(vat_id);
            }
        }
    }

    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::lines;
    use crate::layout::tests::item;

    /// A partial invoice laid out as label/value rows, like most German
    /// invoices.
    fn partial_invoice() -> Vec<Line> {
        lines(&[
            item(1, 50.0, 780.0, "Musterplanung GmbH · Hauptstr. 1 · 86150 Augsburg"),
            item(1, 50.0, 760.0, "USt-IdNr.: DE123456789"),
            item(1, 50.0, 700.0, "Teilrechnung"),
            item(1, 400.0, 700.0, "Musterstadt, den 07.09.2026"),
            item(1, 50.0, 680.0, "Nr.: RE-2026-0042"),
            item(1, 300.0, 680.0, "Kunden-Nr.: 10001"),
            item(1, 50.0, 400.0, "Nettosumme:"),
            item(1, 400.0, 400.0, "3.250,00 €"),
            item(1, 50.0, 385.0, "MwSt. 19,00 %:"),
            item(1, 400.0, 385.0, "617,50 €"),
            item(1, 50.0, 370.0, "Gesamtsumme:"),
            item(1, 400.0, 370.0, "3.867,50 €"),
            item(1, 50.0, 340.0, "Zahlbar bis zum 17.09.2026 ohne Abzug"),
            item(1, 50.0, 100.0, "IBAN: DE89 3704 0044 0532 0130 00"),
        ])
    }

    #[test]
    fn reads_a_partial_invoice() {
        let candidates = extract(&partial_invoice());

        assert_eq!(candidates.document_kind, DocumentKind::Invoice);
        assert_eq!(candidates.invoice_kind, Some(InvoiceKind::Partial));
        assert_eq!(candidates.number.map(|f| f.value), Some("RE-2026-0042".to_owned()));
        assert_eq!(candidates.issue_date.map(|f| f.value), Some("2026-09-07".to_owned()));
        assert_eq!(candidates.due_date.map(|f| f.value), Some("2026-09-17".to_owned()));
        assert_eq!(
            candidates.amounts,
            Amounts {
                net: Some("3250.00".to_owned()),
                tax: Some("617.50".to_owned()),
                gross: Some("3867.50".to_owned()),
                tax_rate: Some(19),
                consistent: true,
            }
        );
        assert_eq!(candidates.ibans, ["DE89370400440532013000"]);
        assert_eq!(candidates.vat_ids, ["DE123456789"]);
    }

    #[test]
    fn pairs_amounts_whose_labels_and_values_sit_in_separate_cells() {
        // Labels stacked in one cell, values in another, as some invoicing
        // tools lay out their totals.
        let lines = lines(&[
            item(1, 50.0, 300.0, "Rechnung"),
            item(1, 300.0, 200.0, "Gesamt (Netto): Steuer 19% (S): Gesamt (Brutto):"),
            item(1, 450.0, 180.0, "2.400,00 € 456,00 € 2.856,00 €"),
        ]);

        let amounts = extract(&lines).amounts;

        assert_eq!(amounts.net.as_deref(), Some("2400.00"));
        assert_eq!(amounts.tax.as_deref(), Some("456.00"));
        assert_eq!(amounts.gross.as_deref(), Some("2856.00"));
    }

    #[test]
    fn finds_a_value_below_its_label() {
        let lines = lines(&[
            item(1, 50.0, 700.0, "Rechnung"),
            item(1, 300.0, 680.0, "Rechnungsdatum:"),
            item(1, 300.0, 665.0, "24.09.2026"),
        ]);

        assert_eq!(
            extract(&lines).issue_date.map(|f| f.value),
            Some("2026-09-24".to_owned())
        );
    }

    #[test]
    fn reads_a_quote_with_its_validity() {
        let lines = lines(&[
            item(1, 50.0, 700.0, "Angebot Nr. AN-2026-117"),
            item(1, 50.0, 680.0, "Angebotsdatum: 08.07.2026"),
            item(1, 50.0, 660.0, "Dieses Angebot ist gültig bis 08.10.2026."),
            item(1, 50.0, 300.0, "Angebotssumme brutto 48.250,00 €"),
        ]);

        let candidates = extract(&lines);

        assert_eq!(candidates.document_kind, DocumentKind::Quote);
        assert_eq!(candidates.invoice_kind, None);
        assert_eq!(candidates.number.map(|f| f.value), Some("AN-2026-117".to_owned()));
        assert_eq!(candidates.issue_date.map(|f| f.value), Some("2026-07-08".to_owned()));
        assert_eq!(candidates.valid_until.map(|f| f.value), Some("2026-10-08".to_owned()));
        assert_eq!(candidates.amounts.gross.as_deref(), Some("48250.00"));
        assert!(!candidates.amounts.consistent);
    }

    #[test]
    fn reads_a_value_below_its_label_in_a_header_row() {
        // The header row is one span; the value row below is one span too.
        let lines = lines(&[
            item(1, 50.0, 700.0, "ANGEBOT"),
            item(1, 50.0, 680.0, "Kundennummer Referenz Belegnummer Datum"),
            item(1, 50.0, 668.0, "4711 BV Mustermann AN-26-0815 08.07.2026"),
            item(1, 50.0, 600.0, "Rückfragen unter der Telefon-Nr. +49 160 1234567"),
        ]);

        let candidates = extract(&lines);

        assert_eq!(candidates.number.map(|f| f.value), Some("AN-26-0815".to_owned()));
        assert_eq!(candidates.issue_date.map(|f| f.value), Some("2026-07-08".to_owned()));
    }

    #[test]
    fn leaves_the_total_empty_without_a_label_or_a_consistent_triple() {
        let lines = lines(&[
            item(1, 50.0, 700.0, "Rechnung"),
            item(1, 50.0, 600.0, "3 Stück à 1,00 €"),
        ]);

        assert_eq!(extract(&lines).amounts.gross, None);
    }

    #[test]
    fn rejects_ibans_with_a_wrong_check_digit() {
        let lines = lines(&[item(1, 50.0, 100.0, "IBAN DE88 3704 0044 0532 0130 00")]);

        assert!(extract(&lines).ibans.is_empty());
    }

    #[test]
    fn classifies_a_credit_note_and_other_documents() {
        assert_eq!(
            extract(&lines(&[item(1, 50.0, 700.0, "Gutschrift zur Rechnung 4711")])).document_kind,
            DocumentKind::CreditNote
        );
        assert_eq!(
            extract(&lines(&[item(1, 50.0, 700.0, "Lieferschein")])).document_kind,
            DocumentKind::Other
        );
    }
}
