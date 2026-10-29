//! Field candidates of invoices and quotes, read from reconstructed lines with
//! the words of a language `Profile`.
//!
//! Everything here is a proposal for a person to confirm: values carry the line
//! they were read from, and absent fields stay `None` rather than being guessed.

use std::ops::Range;

use regex::Regex;
use serde::Serialize;
use utoipa::ToSchema;

use crate::currency;
use crate::dates::{self, Date, DateReader};
use crate::layout::{Line, Span};
use crate::line_items;
use crate::numbers::{self, NumberFormat};
use crate::profile::Profile;
use crate::regex;
use crate::seller;
use crate::tax::{self, Breakdown};

pub use crate::line_items::LineItem;

/// Finds a value in a run of text and returns its byte range.
type Find<'a> = &'a dyn Fn(&str) -> Option<Range<usize>>;

/// How far, in PDF points, a span may start before or end after a label's
/// column and still count as sitting in it.
const COLUMN_MARGIN: f32 = 40.0;

/// A span starting at most this far, in PDF points, left of a label's column
/// is searched whole; one starting further left only from the column on.
const COLUMN_START_MARGIN: f32 = 10.0;

/// How many lines below a label its value is looked for.
const LINES_BELOW_LABEL: usize = 3;

/// Characters the search in a span starts before the estimated column: the
/// proportional position is only an estimate.
const OFFSET_MARGIN: usize = 3;

/// What a document-kind word on the first page, where titles sit, counts
/// against one elsewhere.
const FIRST_PAGE_WEIGHT: usize = 2;

/// What a credit-note word counts against an invoice or quote word on the
/// same line: a credit note names the invoice it corrects.
const CREDIT_NOTE_WEIGHT: usize = 3;

/// What the document is, judged by its title and labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    /// An invoice, standard or partial.
    Invoice,
    /// A quote or estimate.
    Quote,
    /// A credit note, which corrects or cancels an invoice.
    CreditNote,
    /// None of the above, such as a delivery note or a letter.
    Other,
}

/// A standard, partial (`Abschlagsrechnung`, `Teilrechnung`) or final
/// (`Schlussrechnung`) invoice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InvoiceKind {
    /// An invoice that is neither partial nor final.
    Standard,
    /// A partial or progress invoice.
    Partial,
    /// The final invoice that settles the partial ones.
    Final,
}

/// A value and where it was read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Field {
    /// The normalized value; dates as `YYYY-MM-DD`.
    pub value: String,
    /// 1-indexed page number.
    pub page: u32,
    /// The line the value was read from.
    pub line: String,
}

/// The document's totals. Without tax lines that add up to a gross amount,
/// only labelled amounts are returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Amounts {
    /// The net amount as a decimal string with a dot and two fraction
    /// digits, e.g. `3250.00`.
    pub net: Option<String>,
    /// The tax amount, written like `net`.
    pub tax: Option<String>,
    /// The gross amount, written like `net`.
    pub gross: Option<String>,
    /// The ISO 4217 code of the currency the amounts are written in, when the
    /// document names one unambiguously next to its amounts.
    pub currency: Option<String>,
    /// The net and tax amount per tax rate, ordered by rate. Only rates the
    /// document prints are used.
    pub tax_lines: Vec<TaxLine>,
    /// Whether the tax lines add up to the gross amount on the document.
    pub consistent: bool,
}

/// The net and tax amount of one tax rate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct TaxLine {
    /// The rate in percent as printed, e.g. `19`, `7.5` or `8.1`.
    pub rate: String,
    /// The net amount taxed at the rate, as a decimal string with a dot and
    /// two fraction digits.
    pub net: String,
    /// The tax on `net`, written like it.
    pub tax: String,
}

/// Field candidates of an invoice or quote, for a person to confirm. A
/// field that cannot be read with confidence is `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Candidates {
    /// The language profile the document was read with, detected from its
    /// words, e.g. `de` or `en`.
    pub profile: String,
    /// Whether the document is an invoice, a quote, a credit note or none.
    pub document_kind: DocumentKind,
    /// Only for invoices.
    pub invoice_kind: Option<InvoiceKind>,
    /// The invoice or quote number.
    pub number: Option<Field>,
    /// The date the document was issued.
    pub issue_date: Option<Field>,
    /// The date an invoice is payable by.
    pub due_date: Option<Field>,
    /// The end of a quote's validity.
    pub valid_until: Option<Field>,
    /// The document's totals.
    pub amounts: Amounts,
    /// IBANs without spaces, checked with their ISO 13616 check digits.
    pub ibans: Vec<String>,
    /// VAT identification numbers of EU countries, the United Kingdom,
    /// Switzerland and Norway, without spaces, dots or dashes.
    pub vat_ids: Vec<String>,
    /// The issuing company, by its name with a legal form.
    pub seller_name: Option<Field>,
    /// The positions of the document; empty when it has fewer than two.
    pub line_items: Vec<LineItem>,
}

/// The field candidates of a document's `lines`, read with the words of
/// `profile`.
pub(crate) fn extract(lines: &[Line], profile: &Profile) -> Candidates {
    let labels = &profile.labels;
    let numbers = NumberFormat::detect(lines, profile.decimal_mark);
    let reader = DateReader::new(lines, &profile.dates);
    let document_kind = document_kind(lines, profile);
    let number_label = match document_kind {
        DocumentKind::Quote => &labels.quote_number,
        _ => &labels.invoice_number,
    };

    let find_date = |text: &str| reader.find(text);
    // Without a label, the first date on a line that no other date label
    // claims is the issue date.
    let unclaimed: Vec<&Line> = lines
        .iter()
        .filter(|line| {
            let text = line.text();
            !labels.due_date.is_match(text) && !labels.valid_until.is_match(text)
        })
        .collect();
    let issue = labelled(lines, &labels.issue_date, &find_date).or_else(|| first_match(&unclaimed, &find_date));
    let due = labelled(lines, &labels.due_date, &find_date);
    let valid_until = labelled(lines, &labels.valid_until, &find_date);
    let read = |field: &Option<Field>| field.as_ref().and_then(|field| reader.read(&field.value));
    let (issued, due_on) = dates::settle(read(&issue), read(&due));
    let (issued_for_quote, valid_to) = dates::settle(read(&issue), read(&valid_until));
    let all_amounts: Vec<i64> = lines
        .iter()
        .flat_map(Line::spans)
        .flat_map(|span| numbers.amounts(&span.text).map(|(_, cents)| cents).collect::<Vec<_>>())
        .collect();
    let breakdown = tax::breakdown(&all_amounts, &tax::rates(lines));
    let rates: Vec<tax::Rate> = breakdown
        .iter()
        .flat_map(|breakdown| breakdown.lines.iter().map(|line| line.rate))
        .collect();

    Candidates {
        profile: profile.id.clone(),
        document_kind,
        invoice_kind: (document_kind == DocumentKind::Invoice).then(|| invoice_kind(lines, profile)),
        number: labelled(lines, number_label, &|text| {
            NUMBER.find(text).map(|found| found.range())
        }),
        issue_date: dated(issue, issued.or(issued_for_quote)),
        due_date: dated(due, due_on),
        valid_until: dated(valid_until, valid_to),
        amounts: amounts(lines, profile, numbers, breakdown),
        ibans: ibans(lines),
        vat_ids: vat_ids(lines),
        seller_name: seller::extract(lines, profile),
        line_items: line_items::extract(lines, profile, numbers, &rates),
    }
}

/// The field with its date as ISO `YYYY-MM-DD`, or none when the date could
/// not be read with confidence.
fn dated(field: Option<Field>, date: Option<Date>) -> Option<Field> {
    let date = date?;

    field.map(|field| Field {
        value: date.iso(),
        ..field
    })
}

regex!(NUMBER, r"\b[A-Z0-9][A-Z0-9\-/\.]*\d[A-Z0-9\-/]*\b");
regex!(IBAN, r"\b[A-Z]{2}\d{2}(?: ?[A-Z0-9]{4}){2,7}(?: ?[A-Z0-9]{1,4})?\b");
// The formats of VAT identification numbers in the EU, the United Kingdom,
// Switzerland and Norway, with the spaces and dots they are often printed with.
regex!(
    VAT_ID,
    r"(?x)\b(?:
        ATU\s?\d{8}
      | BE\s?[01]\d{3}[.\s]?\d{3}[.\s]?\d{3}
      | BG\s?\d{9,10}
      | CHE[-\s]?\d{3}[.\s]?\d{3}[.\s]?\d{3}(?:\s?(?:MWST|TVA|IVA))?
      | CY\s?\d{8}[A-Z]
      | CZ\s?\d{8,10}
      | DE\s?\d{9}
      | DK\s?\d{2}\s?\d{2}\s?\d{2}\s?\d{2}
      | EE\s?\d{9}
      | EL\s?\d{9}
      | ES\s?[A-Z0-9]\d{7}[A-Z0-9]
      | FI\s?\d{8}
      | FR\s?[A-HJ-NP-Z0-9]{2}\s?\d{3}\s?\d{3}\s?\d{3}
      | GB\s?(?:\d{3}\s?\d{4}\s?\d{2}(?:\s?\d{3})?|GD\d{3}|HA\d{3})
      | HR\s?\d{11}
      | HU\s?\d{8}
      | IE\s?(?:\d{7}[A-W][A-I]?|\d[A-Z+*]\d{5}[A-W])
      | IT\s?\d{11}
      | LT\s?(?:\d{12}|\d{9})
      | LU\s?\d{8}
      | LV\s?\d{11}
      | MT\s?\d{8}
      | NL\s?\d{9}B\d{2}
      | NO\s?\d{9}\s?MVA
      | PL\s?\d{10}
      | PT\s?\d{9}
      | RO\s?\d{6,10}
      | SE\s?\d{10}01
      | SI\s?\d{8}
      | SK\s?\d{10}
      | XI\s?\d{3}\s?\d{4}\s?\d{2}
    )\b"
);

/// Scores keywords; the first page, where titles sit, counts
/// [`FIRST_PAGE_WEIGHT`] times.
fn document_kind(lines: &[Line], profile: &Profile) -> DocumentKind {
    let words = &profile.document_kind;
    let mut invoice = 0;
    let mut quote = 0;
    let mut credit = 0;

    for line in lines {
        let text = line.text().to_lowercase();
        let weight = if line.page == 1 { FIRST_PAGE_WEIGHT } else { 1 };

        for keyword in &words.invoice {
            if text.contains(keyword.as_str()) {
                invoice += weight;
            }
        }
        for keyword in &words.quote {
            if text.contains(keyword.as_str()) {
                quote += weight;
            }
        }
        if words.credit_note.iter().any(|keyword| text.contains(keyword.as_str())) {
            credit += weight * CREDIT_NOTE_WEIGHT;
        }
    }

    match (invoice, quote, credit) {
        (0, 0, 0) => DocumentKind::Other,
        (_, _, c) if c >= invoice && c >= quote => DocumentKind::CreditNote,
        (i, q, _) if q > i => DocumentKind::Quote,
        _ => DocumentKind::Invoice,
    }
}

fn invoice_kind(lines: &[Line], profile: &Profile) -> InvoiceKind {
    let words = &profile.invoice_kind;
    let first_page: String = lines
        .iter()
        .filter(|line| line.page == 1)
        .map(|line| line.text().to_lowercase())
        .collect::<Vec<_>>()
        .join("\n");

    if words.r#final.iter().any(|word| first_page.contains(word.as_str())) {
        InvoiceKind::Final
    } else if words.partial.iter().any(|word| first_page.contains(word.as_str())) {
        InvoiceKind::Partial
    } else {
        InvoiceKind::Standard
    }
}

/// The first value matching `value` after a `label`: in the rest of the
/// label's span, then in the spans to its right, then in the same column on
/// the next [`LINES_BELOW_LABEL`] lines.
fn labelled(lines: &[Line], label: &Regex, value: Find<'_>) -> Option<Field> {
    for (index, line) in lines.iter().enumerate() {
        for (span_index, span) in line.spans().iter().enumerate() {
            let Some(found) = label.find(&span.text) else {
                continue;
            };

            let rest = &span.text[found.end()..];
            let right = line.spans()[span_index + 1..].iter().map(|span| span.text.as_str());

            for candidate in std::iter::once(rest).chain(right) {
                if let Some(range) = value(candidate) {
                    return Some(field(line, &candidate[range]));
                }
            }

            // Several labels can share one span, as in a table header row; the
            // label's own column is estimated from its offset in the span.
            let column = label_column(span, found.start(), found.end());

            for below in lines[index + 1..]
                .iter()
                .take(LINES_BELOW_LABEL)
                .filter(|below| below.page == line.page)
            {
                if let Some(matched) = below
                    .spans()
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

fn first_match(lines: &[&Line], value: Find<'_>) -> Option<Field> {
    lines.iter().find_map(|line| {
        let text = line.text();
        value(text).map(|range| field(line, &text[range]))
    })
}

fn field(line: &Line, value: &str) -> Field {
    Field {
        value: value.trim().to_owned(),
        page: line.page,
        line: line.text().to_owned(),
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
fn value_in_column(candidate: &Span, column: &Span, value: Find<'_>) -> Option<String> {
    let chars = candidate.text.chars().count();
    let offset = if candidate.x >= column.x - COLUMN_START_MARGIN || candidate.width <= 0.0 {
        0
    } else {
        let ratio = ((column.x - candidate.x) / candidate.width).clamp(0.0, 1.0);
        // `ratio` is clamped, so the estimate is never negative.
        let estimated = usize::try_from((ratio * chars as f32) as i64).unwrap_or(0);
        estimated.saturating_sub(OFFSET_MARGIN)
    };
    let byte_offset = candidate
        .text
        .char_indices()
        .nth(offset)
        .map_or(candidate.text.len(), |(index, _)| index);

    let rest = &candidate.text[byte_offset..];

    value(rest).map(|range| rest[range].to_owned())
}

/// Whether `candidate` sits in the column of `label`, give or take
/// [`COLUMN_MARGIN`].
fn overlaps(label: &Span, candidate: &Span) -> bool {
    candidate.x <= label.right() + COLUMN_MARGIN && candidate.right() >= label.x - COLUMN_MARGIN
}

fn amounts(lines: &[Line], profile: &Profile, numbers: NumberFormat, breakdown: Option<Breakdown>) -> Amounts {
    let currency = currency::detect(lines);

    if let Some(breakdown) = breakdown {
        return Amounts {
            net: Some(numbers::decimal(breakdown.net)),
            tax: Some(numbers::decimal(breakdown.tax)),
            gross: Some(numbers::decimal(breakdown.gross)),
            currency,
            tax_lines: breakdown
                .lines
                .iter()
                .map(|line| TaxLine {
                    rate: line.rate.decimal(),
                    net: numbers::decimal(line.net),
                    tax: numbers::decimal(line.tax),
                })
                .collect(),
            consistent: true,
        };
    }

    // Which currency a labelled amount is in is open when there are several.
    let labelled_cents = |label: &Regex| {
        labelled(lines, label, &|text| numbers.find_amount(text))
            .and_then(|field| numbers.whole_amount(&field.value))
            .filter(|_| !currency::several(lines))
    };
    // No largest-amount fallback: a wrong total is worse than none.
    let gross = labelled_cents(&profile.labels.gross);
    // A subtotal that is not below the total was taken after tax.
    let net = labelled_cents(&profile.labels.net).filter(|net| gross.is_none_or(|gross| *net < gross));

    Amounts {
        net: net.map(numbers::decimal),
        tax: None,
        gross: gross.map(numbers::decimal),
        currency,
        tax_lines: Vec::new(),
        consistent: false,
    }
}

fn ibans(lines: &[Line]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();

    for line in lines {
        for matched in IBAN.find_iter(line.text()) {
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
        for matched in VAT_ID.find_iter(line.text()) {
            let mut vat_id: String = matched.as_str().chars().filter(char::is_ascii_alphanumeric).collect();
            // A Swiss number names its tax after the digits.
            if vat_id.starts_with("CHE") {
                vat_id.truncate(12);
            }

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
    use crate::test_support::{english, german, item};

    fn extract(lines: &[Line]) -> Candidates {
        super::extract(lines, german())
    }

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
                currency: Some("EUR".to_owned()),
                tax_lines: vec![TaxLine {
                    rate: "19".to_owned(),
                    net: "3250.00".to_owned(),
                    tax: "617.50".to_owned(),
                }],
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
    fn splits_the_totals_of_an_invoice_with_two_rates() {
        let lines = lines(&[
            item(1, 50.0, 700.0, "Rechnung"),
            item(1, 50.0, 400.0, "Nettobetrag"),
            item(1, 400.0, 400.0, "150,00 €"),
            item(1, 50.0, 385.0, "zzgl. 7 % USt. auf 50,00 €"),
            item(1, 400.0, 385.0, "3,50 €"),
            item(1, 50.0, 370.0, "zzgl. 19 % USt. auf 100,00 €"),
            item(1, 400.0, 370.0, "19,00 €"),
            item(1, 50.0, 355.0, "Rechnungsbetrag"),
            item(1, 400.0, 355.0, "172,50 €"),
            item(1, 50.0, 300.0, "Bei Zahlung binnen 10 Tagen 2 % Skonto."),
        ]);

        let amounts = extract(&lines).amounts;

        assert_eq!(amounts.gross.as_deref(), Some("172.50"));
        assert_eq!(amounts.tax.as_deref(), Some("22.50"));
        assert_eq!(
            amounts
                .tax_lines
                .iter()
                .map(|line| (line.rate.as_str(), line.net.as_str(), line.tax.as_str()))
                .collect::<Vec<_>>(),
            [("7", "50.00", "3.50"), ("19", "100.00", "19.00")]
        );
    }

    #[test]
    fn leaves_the_tax_lines_empty_without_a_printed_rate() {
        // 19 % of the net, but the document prints no rate.
        let lines = lines(&[
            item(1, 50.0, 700.0, "Rechnung"),
            item(1, 50.0, 400.0, "Nettosumme: 3.250,00 €"),
            item(1, 50.0, 385.0, "Umsatzsteuer: 617,50 €"),
            item(1, 50.0, 370.0, "Gesamtsumme: 3.867,50 €"),
        ]);

        let amounts = extract(&lines).amounts;

        assert!(amounts.tax_lines.is_empty());
        assert!(!amounts.consistent);
        assert_eq!(amounts.gross.as_deref(), Some("3867.50"));
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
    fn reads_vat_ids_of_other_countries_as_printed() {
        let lines = lines(&[
            item(1, 50.0, 700.0, "VAT Reg No: GB 674 4257 93"),
            item(1, 50.0, 680.0, "UID: CHE-123.456.789 MWST"),
            item(1, 50.0, 660.0, "BTW-nr: NL123456789B01"),
            item(1, 50.0, 640.0, "TVA: FR 12 345 678 901"),
            item(1, 50.0, 620.0, "IBAN: GB29 NWBK 6016 1331 9268 19"),
        ]);

        assert_eq!(
            extract(&lines).vat_ids,
            ["GB674425793", "CHE123456789", "NL123456789B01", "FR12345678901"]
        );
    }

    #[test]
    fn reads_totals_written_with_a_decimal_point() {
        // An invoice from a country that writes `1,962.00`, with German labels.
        let lines = lines(&[
            item(1, 50.0, 700.0, "Rechnung"),
            item(1, 50.0, 400.0, "Nettosumme: 1,962.00 €"),
            item(1, 50.0, 385.0, "MwSt. 19%: 372.78 €"),
            item(1, 50.0, 370.0, "Gesamtsumme: 2,334.78 €"),
        ]);

        let amounts = extract(&lines).amounts;

        assert_eq!(amounts.net.as_deref(), Some("1962.00"));
        assert_eq!(amounts.gross.as_deref(), Some("2334.78"));
        assert_eq!(amounts.currency.as_deref(), Some("EUR"));
        assert!(amounts.consistent);
    }

    #[test]
    fn settles_slashed_issue_and_due_dates_by_their_order() {
        // 09/03 and 08/04 are both valid either way; only day first puts the
        // due date after the issue date.
        let lines = lines(&[
            item(1, 50.0, 700.0, "Rechnung"),
            item(1, 50.0, 680.0, "Rechnungsdatum: 09/03/2026"),
            item(1, 50.0, 660.0, "Zahlbar bis: 08/04/2026"),
        ]);

        let candidates = extract(&lines);

        assert_eq!(candidates.issue_date.map(|f| f.value), Some("2026-03-09".to_owned()));
        assert_eq!(candidates.due_date.map(|f| f.value), Some("2026-04-08".to_owned()));
    }

    #[test]
    fn leaves_an_ambiguous_slashed_date_empty() {
        let lines = lines(&[
            item(1, 50.0, 700.0, "Rechnung"),
            item(1, 50.0, 680.0, "Rechnungsdatum: 02/03/2026"),
        ]);

        assert_eq!(extract(&lines).issue_date, None);
    }

    #[test]
    fn takes_no_due_date_for_an_unlabelled_issue_date() {
        // The due date comes first in reading order; the issue date has no
        // label the profile knows.
        let lines = lines(&[
            item(1, 50.0, 700.0, "Rechnung"),
            item(1, 50.0, 680.0, "Zahlbar bis 08.10.2026"),
            item(1, 400.0, 660.0, "Musterstadt, 08.09.2026"),
        ]);

        let candidates = extract(&lines);

        assert_eq!(candidates.issue_date.map(|f| f.value), Some("2026-09-08".to_owned()));
        assert_eq!(candidates.due_date.map(|f| f.value), Some("2026-10-08".to_owned()));
    }

    #[test]
    fn reads_an_issue_date_with_a_written_month() {
        let lines = lines(&[
            item(1, 50.0, 700.0, "Rechnung"),
            item(1, 400.0, 700.0, "Musterstadt, den 7. September 2026"),
        ]);

        assert_eq!(
            extract(&lines).issue_date.map(|f| f.value),
            Some("2026-09-07".to_owned())
        );
    }

    #[test]
    fn names_no_currency_for_a_bare_dollar_sign() {
        let lines = lines(&[
            item(1, 50.0, 700.0, "Rechnung"),
            item(1, 50.0, 400.0, "Gesamtsumme: $346.50"),
        ]);

        let amounts = extract(&lines).amounts;

        assert_eq!(amounts.gross.as_deref(), Some("346.50"));
        assert_eq!(amounts.currency, None);
    }

    #[test]
    fn reads_an_english_invoice() {
        // Header fields in a right-hand column, slashed dates, totals at 20 %.
        let lines = lines(&[
            item(1, 40.0, 800.0, "Riverbend Joinery Limited"),
            item(1, 420.0, 800.0, "TAX INVOICE"),
            item(1, 330.0, 770.0, "Invoice No.:"),
            item(1, 450.0, 770.0, "RJ-20417"),
            item(1, 330.0, 755.0, "Date:"),
            item(1, 450.0, 755.0, "14/09/2026"),
            item(1, 330.0, 740.0, "Due Date:"),
            item(1, 450.0, 740.0, "14/10/2026"),
            item(1, 40.0, 700.0, "Bill To"),
            item(1, 40.0, 400.0, "Oak worktop, fitted"),
            item(1, 450.0, 400.0, "1,250.00"),
            item(1, 330.0, 300.0, "Subtotal"),
            item(1, 450.0, 300.0, "£1,250.00"),
            item(1, 330.0, 285.0, "VAT 20%"),
            item(1, 450.0, 285.0, "£250.00"),
            item(1, 330.0, 270.0, "TOTAL"),
            item(1, 450.0, 270.0, "£1,500.00"),
        ]);

        let candidates = super::extract(&lines, english());

        assert_eq!(candidates.profile, "en");
        assert_eq!(candidates.document_kind, DocumentKind::Invoice);
        assert_eq!(candidates.number.map(|f| f.value), Some("RJ-20417".to_owned()));
        assert_eq!(candidates.issue_date.map(|f| f.value), Some("2026-09-14".to_owned()));
        assert_eq!(candidates.due_date.map(|f| f.value), Some("2026-10-14".to_owned()));
        assert_eq!(candidates.amounts.gross.as_deref(), Some("1500.00"));
        assert_eq!(candidates.amounts.currency.as_deref(), Some("GBP"));
        assert_eq!(
            candidates.seller_name.map(|f| f.value),
            Some("Riverbend Joinery Limited".to_owned())
        );
    }

    #[test]
    fn reads_an_english_date_with_an_ordinal_day() {
        let lines = lines(&[
            item(1, 50.0, 700.0, "Quotation No: Q-311"),
            item(1, 50.0, 680.0, "Date: 3rd March 2026"),
            item(1, 50.0, 660.0, "Valid until: March 31st, 2026"),
        ]);

        let candidates = super::extract(&lines, english());

        assert_eq!(candidates.document_kind, DocumentKind::Quote);
        assert_eq!(candidates.number.map(|f| f.value), Some("Q-311".to_owned()));
        assert_eq!(candidates.issue_date.map(|f| f.value), Some("2026-03-03".to_owned()));
        assert_eq!(candidates.valid_until.map(|f| f.value), Some("2026-03-31".to_owned()));
    }

    #[test]
    fn takes_no_subtotal_that_includes_the_tax() {
        let lines = lines(&[
            item(1, 50.0, 700.0, "Invoice"),
            item(1, 330.0, 300.0, "Subtotal"),
            item(1, 450.0, 300.0, "2,300.00"),
            item(1, 330.0, 285.0, "Amount Due"),
            item(1, 450.0, 285.0, "2,300.00"),
        ]);

        let amounts = super::extract(&lines, english()).amounts;

        assert_eq!(amounts.net, None);
        assert_eq!(amounts.gross.as_deref(), Some("2300.00"));
    }

    #[test]
    fn reads_no_labelled_totals_in_two_currencies() {
        let lines = lines(&[
            item(1, 50.0, 700.0, "Invoice"),
            item(1, 330.0, 300.0, "Subtotal US$ 180.00"),
            item(1, 330.0, 285.0, "Amount Due ZAR 3,321.22"),
        ]);

        let amounts = super::extract(&lines, english()).amounts;

        assert_eq!((amounts.net, amounts.gross), (None, None));
    }

    #[test]
    fn takes_no_line_item_column_for_the_total() {
        // `Total` heads the amount column; only a labelled total counts.
        let lines = lines(&[
            item(1, 50.0, 700.0, "Invoice"),
            item(1, 450.0, 600.0, "Total"),
            item(1, 450.0, 585.0, "120.00"),
        ]);

        assert_eq!(super::extract(&lines, english()).amounts.gross, None);
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
