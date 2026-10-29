//! Golden test of the field candidates against the CII XML embedded in the
//! Sinabis e-invoices (`tests/fixtures/sinabis-einvoices`).
//!
//! The service returns the XML unchanged and never parses it; this test
//! parses the standalone XML of every PDF/XML pair and compares docpipe's
//! candidates from the PDF's text layer with it. Run with `--nocapture` to
//! see the accuracy table and the mismatches of every field.
//!
//! Thresholds are the accuracies measured on 2026-10-29, rounded down to the
//! nearest 5 %, so a regression fails and an improvement passes; raise them
//! when the heuristics improve. "Wrong" counts documents whose candidate
//! differs from the XML: precision matters more than recall here, since a
//! field should stay empty rather than hold a guess.

#![allow(clippy::print_stdout, clippy::unwrap_used)]

mod common;

use std::collections::BTreeMap;
use std::fs;

use docpipe::analyze::analyze;
use docpipe::candidates::{Candidates, DocumentKind, Field};
use docpipe::ocr::OcrSettings;
use roxmltree::{Document, Node};

/// Minimum accuracy per field, in percent: measured value rounded down to 5.
///
/// Measured 2026-10-29 over 100 documents (correct / missing / wrong):
/// `document_kind` 96 / 0 / 4, `number` 93 / 0 / 7, `issue_date` 99 / 0 / 1,
/// `due_date` 86 / 14 / 0, `gross` 94 / 4 / 2, `net` 83 / 12 / 5, `tax` 81 / 19 / 0,
/// `seller_vat_id` 79 / 15 / 4 of 98 with a VAT ID, `seller_name` 73 / 2 / 25,
/// `line_item_count` 45 / 55 / 0.
const MIN_ACCURACY: &[(&str, usize)] = &[
    ("document_kind", 95),
    ("number", 90),
    ("issue_date", 95),
    ("due_date", 85),
    ("gross", 90),
    ("net", 80),
    ("tax", 80),
    ("seller_vat_id", 80),
    ("seller_name", 70),
    ("line_item_count", 45),
];

/// Maximum share of documents with a confidently wrong candidate, in
/// percent: the value measured on 2026-10-29 (`number` 7, `issue_date` 1,
/// `due_date` 0, `gross` 2) plus 5.
const MAX_WRONG: &[(&str, usize)] = &[("number", 12), ("issue_date", 6), ("due_date", 5), ("gross", 7)];

/// The reference fields of one e-invoice, read from its CII XML.
struct Expected {
    kind: DocumentKind,
    number: String,
    issue_date: String,
    due_date: Option<String>,
    gross: Option<i64>,
    net: Option<i64>,
    tax: Option<i64>,
    seller_vat_id: Option<String>,
    seller_name: String,
    line_items: usize,
}

fn child<'a>(node: Node<'a, 'a>, name: &str) -> Option<Node<'a, 'a>> {
    node.children()
        .find(|child| child.is_element() && child.tag_name().name() == name)
}

fn descendant<'a>(node: Node<'a, 'a>, name: &str) -> Option<Node<'a, 'a>> {
    node.descendants()
        .find(|descendant| descendant.is_element() && descendant.tag_name().name() == name)
}

fn text(node: Option<Node<'_, '_>>) -> Option<String> {
    node.and_then(|node| node.text())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// `20210826` (UN/CEFACT format 102) as `2021-08-26`.
fn iso_date(node: Option<Node<'_, '_>>) -> Option<String> {
    let raw = text(descendant(node?, "DateTimeString"))?;
    assert_eq!(raw.len(), 8, "unexpected date format {raw}");
    Some(format!("{}-{}-{}", &raw[..4], &raw[4..6], &raw[6..]))
}

/// A decimal amount as cents, so `1234.5`, `1234.50` and `1234.500` agree.
fn cents(amount: &str) -> Option<i64> {
    let (sign, digits) = match amount.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, amount),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let whole: i64 = whole.parse().ok()?;
    let fraction = format!("{fraction:0<2}");
    if fraction.len() > 2 && fraction[2..].bytes().any(|byte| byte != b'0') {
        return None;
    }
    let fraction: i64 = fraction[..2].parse().ok()?;
    Some(sign * (whole * 100 + fraction))
}

fn expected(xml: &str) -> Expected {
    let document = Document::parse(xml).unwrap();
    let root = document.root_element();
    let header = descendant(root, "ExchangedDocument").unwrap();
    let kind = match text(child(header, "TypeCode")).as_deref() {
        Some("381") => DocumentKind::CreditNote,
        // 380 invoice, 875/877 partial and final construction invoices.
        _ => DocumentKind::Invoice,
    };
    let totals = descendant(root, "SpecifiedTradeSettlementHeaderMonetarySummation").unwrap();
    let amount = |name| text(child(totals, name)).map(|value| cents(&value).unwrap());
    let seller = descendant(root, "SellerTradeParty").unwrap();
    let seller_vat_id = seller
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "ID" && node.attribute("schemeID") == Some("VA"))
        .find_map(|node| text(Some(node)));
    Expected {
        kind,
        number: text(child(header, "ID")).unwrap(),
        issue_date: iso_date(child(header, "IssueDateTime")).unwrap(),
        due_date: iso_date(descendant(root, "DueDateDateTime")),
        gross: amount("GrandTotalAmount"),
        net: amount("TaxBasisTotalAmount"),
        tax: amount("TaxTotalAmount"),
        seller_vat_id,
        seller_name: text(child(seller, "Name")).unwrap(),
        line_items: root
            .descendants()
            .filter(|node| node.is_element() && node.tag_name().name() == "IncludedSupplyChainTradeLineItem")
            .count(),
    }
}

/// How one candidate compares with the reference.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The candidate equals the reference.
    Correct,
    /// No candidate, though the reference has a value.
    Missing,
    /// The candidate differs from the reference.
    Wrong,
    /// Neither the reference nor the candidate has a value.
    Absent,
}

#[derive(Default)]
struct Tally {
    correct: usize,
    missing: usize,
    wrong: usize,
    absent: usize,
    mismatches: Vec<String>,
}

impl Tally {
    fn record(&mut self, file: &str, outcome: Outcome, expected: &str, got: &str) {
        match outcome {
            Outcome::Correct => self.correct += 1,
            Outcome::Missing => self.missing += 1,
            Outcome::Wrong => self.wrong += 1,
            Outcome::Absent => self.absent += 1,
        }
        if matches!(outcome, Outcome::Missing | Outcome::Wrong) {
            self.mismatches
                .push(format!("  {file}: expected {expected:?}, got {got:?}"));
        }
    }

    /// Documents whose reference has a value.
    fn applicable(&self) -> usize {
        self.correct + self.missing + self.wrong
    }

    fn accuracy(&self) -> usize {
        self.correct * 100 / self.applicable().max(1)
    }
}

fn compare<T: PartialEq>(expected: Option<&T>, got: Option<&T>) -> Outcome {
    match (expected, got) {
        (Some(expected), Some(got)) if expected == got => Outcome::Correct,
        (Some(_) | None, Some(_)) => Outcome::Wrong,
        (Some(_), None) => Outcome::Missing,
        (None, None) => Outcome::Absent,
    }
}

fn describe<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "-".to_string(), |value| value.to_string())
}

/// A candidate with the line it was read from, or `-`.
fn shown(field: Option<&Field>) -> String {
    field.map_or_else(
        || "-".to_string(),
        |field| format!("{} (p{} {:?})", field.value, field.page, field.line),
    )
}

/// Cents as a decimal string, `6622350` as `66223.50`.
fn decimal(cents: i64) -> String {
    format!(
        "{}{}.{:02}",
        if cents < 0 { "-" } else { "" },
        cents.abs() / 100,
        cents.abs() % 100
    )
}

/// Company names compared without case and with single spaces.
fn normalized_name(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn compare_document(
    file: &str,
    expected: &Expected,
    candidates: &Candidates,
    tallies: &mut BTreeMap<&'static str, Tally>,
) {
    let mut record = |field: &'static str, outcome: Outcome, expected: String, got: String| {
        tallies.entry(field).or_default().record(file, outcome, &expected, &got);
    };

    let kind = if expected.kind == candidates.document_kind {
        Outcome::Correct
    } else {
        Outcome::Wrong
    };
    record(
        "document_kind",
        kind,
        format!("{:?}", expected.kind),
        format!("{:?}", candidates.document_kind),
    );

    let number = candidates.number.as_ref().map(|field| field.value.clone());
    record(
        "number",
        compare(Some(&expected.number), number.as_ref()),
        expected.number.clone(),
        shown(candidates.number.as_ref()),
    );

    let issue_date = candidates.issue_date.as_ref().map(|field| field.value.clone());
    record(
        "issue_date",
        compare(Some(&expected.issue_date), issue_date.as_ref()),
        expected.issue_date.clone(),
        shown(candidates.issue_date.as_ref()),
    );

    let due_date = candidates.due_date.as_ref().map(|field| field.value.clone());
    record(
        "due_date",
        compare(expected.due_date.as_ref(), due_date.as_ref()),
        describe(expected.due_date.as_ref()),
        shown(candidates.due_date.as_ref()),
    );

    for (field, reference, candidate) in [
        ("gross", expected.gross, candidates.amounts.gross.as_deref()),
        ("net", expected.net, candidates.amounts.net.as_deref()),
        ("tax", expected.tax, candidates.amounts.tax.as_deref()),
    ] {
        let got = candidate.map(|value| cents(value).unwrap_or_else(|| panic!("{file}: {field} {value:?}")));
        record(
            field,
            compare(reference.as_ref(), got.as_ref()),
            describe(reference.map(decimal)),
            describe(candidate),
        );
    }

    // The seller's VAT ID among the document's VAT IDs; the buyer's may be
    // listed too.
    let vat_outcome = match &expected.seller_vat_id {
        Some(id) if candidates.vat_ids.contains(id) => Outcome::Correct,
        Some(_) if candidates.vat_ids.is_empty() => Outcome::Missing,
        Some(_) => Outcome::Wrong,
        None => Outcome::Absent,
    };
    record(
        "seller_vat_id",
        vat_outcome,
        describe(expected.seller_vat_id.as_ref()),
        candidates.vat_ids.join(", "),
    );

    let seller_name = candidates.seller_name.as_ref().map(|field| field.value.clone());
    record(
        "seller_name",
        compare(
            Some(&normalized_name(&expected.seller_name)),
            seller_name.as_deref().map(normalized_name).as_ref(),
        ),
        expected.seller_name.clone(),
        shown(candidates.seller_name.as_ref()),
    );

    let count = candidates.line_items.len();
    let line_outcome = if count == expected.line_items {
        Outcome::Correct
    } else if count == 0 {
        Outcome::Missing
    } else {
        Outcome::Wrong
    };
    record(
        "line_item_count",
        line_outcome,
        expected.line_items.to_string(),
        count.to_string(),
    );
}

#[test]
fn candidates_agree_with_the_embedded_cii_xml() {
    let directory = common::fixtures("sinabis-einvoices");
    let records = common::rows(&directory);
    assert_eq!(records.len(), 100);

    let mut tallies: BTreeMap<&'static str, Tally> = BTreeMap::new();
    let mut without_candidates = Vec::new();
    for record in &records {
        let file = record["file"].as_str().unwrap();
        let xml = fs::read_to_string(directory.join(record["xml_file"].as_str().unwrap())).unwrap();
        let expected = expected(&xml);
        assert_eq!(
            expected.number, record["invoice_number"],
            "{file}: XML and manifest disagree"
        );
        assert_eq!(
            expected.issue_date, record["issue_date"],
            "{file}: XML and manifest disagree"
        );
        assert_eq!(
            expected.line_items,
            record["lines_count"].as_u64().unwrap() as usize,
            "{file}"
        );

        let analysis = analyze(&fs::read(directory.join(file)).unwrap(), &OcrSettings::default())
            .unwrap_or_else(|error| panic!("{file}: {error}"));
        let Some(candidates) = &analysis.candidates else {
            without_candidates.push(file.to_string());
            continue;
        };
        compare_document(file, &expected, candidates, &mut tallies);
    }

    let total = records.len();
    println!("\nSinabis e-invoices: candidates vs. embedded CII XML ({total} documents)");
    println!(
        "{:<16} {:>10} {:>8} {:>8} {:>6} {:>9} {:>7}",
        "field", "applicable", "correct", "missing", "wrong", "accuracy", "wrong%"
    );
    for (field, tally) in &tallies {
        println!(
            "{field:<16} {:>10} {:>8} {:>8} {:>6} {:>8}% {:>6}%",
            tally.applicable(),
            tally.correct,
            tally.missing,
            tally.wrong,
            tally.accuracy(),
            tally.wrong * 100 / total,
        );
    }
    if !without_candidates.is_empty() {
        println!("\nwithout candidates: {}", without_candidates.join(", "));
    }
    for (field, tally) in &tallies {
        if !tally.mismatches.is_empty() {
            println!("\n{field} mismatches:");
            for mismatch in &tally.mismatches {
                println!("{mismatch}");
            }
        }
    }

    assert!(
        without_candidates.is_empty(),
        "no candidates for {without_candidates:?}"
    );
    for (field, minimum) in MIN_ACCURACY {
        let tally = &tallies[field];
        assert!(
            tally.accuracy() >= *minimum,
            "{field}: accuracy {}% fell below {minimum}%",
            tally.accuracy()
        );
    }
    for (field, maximum) in MAX_WRONG {
        let wrong = tallies[field].wrong * 100 / total;
        assert!(
            wrong <= *maximum,
            "{field}: {wrong}% of the documents got a wrong value, more than {maximum}%"
        );
    }
}
