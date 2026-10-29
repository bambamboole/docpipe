//! The name of the company that issued a document, read from reconstructed
//! lines.
//!
//! A vendor names itself with its legal form: in the sender line above the
//! address window, in the page footer and in its terms. Every company name
//! with a legal form is a candidate; the one named most often wins, and the
//! first lines of the first page, where the sender line sits, count extra.
//! A private customer has no legal form and is never a candidate. Names are
//! read within one text run, so a title beside the sender line, such as
//! `INVOICE`, is no part of the name. A name under the heading of the
//! recipient's address (`Bill To`), in its column, is the customer's.

use std::collections::HashMap;

use crate::candidates::Field;
use crate::layout::{Line, Span};
use crate::profile::Profile;

/// Lines at the top of the first page that hold the sender line.
const HEAD_LINES: usize = 8;

/// What a mention in the head counts, against one for any other mention.
const HEAD_WEIGHT: usize = 3;

/// The lines under a recipient heading that hold the recipient's name.
const RECIPIENT_LINES: usize = 3;

/// How far, in PDF points, a name may sit from the column of its heading.
const RECIPIENT_COLUMN: f32 = 20.0;

/// The company named with a legal form most often on the document, with the
/// line it was first read from.
pub fn extract(lines: &[Line], profile: &Profile) -> Option<Field> {
    let words = &profile.seller;
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut by_key: HashMap<String, usize> = HashMap::new();
    // The page, line index and x of each recipient heading.
    let headings: Vec<(u32, usize, f32)> = lines
        .iter()
        .enumerate()
        .flat_map(|(index, line)| {
            line.spans()
                .iter()
                .filter(|span| words.recipient.is_match(&span.text))
                .map(move |span| (line.page, index, span.x))
        })
        .collect();
    let is_recipient = |index: usize, line: &Line, span: &Span| {
        words.recipient.is_match(&span.text)
            || headings.iter().any(|&(page, heading, x)| {
                page == line.page
                    && heading < index
                    && index - heading <= RECIPIENT_LINES
                    && (span.x - x).abs() <= RECIPIENT_COLUMN
            })
    };

    for (index, line) in lines.iter().enumerate() {
        let text = line.text();
        let weight = if line.page == 1 && index < HEAD_LINES {
            HEAD_WEIGHT
        } else {
            1
        };

        // Per span: a legal form that counts only at the end of a name ends
        // the span when a title sits to its right on the same line.
        let spans = line.spans().iter().filter(|span| !is_recipient(index, line, span));

        for matched in spans.flat_map(|span| words.company.find_iter(&span.text)) {
            let name = words
                .leading_words
                .replace(
                    matched
                        .as_str()
                        .trim_end_matches([' ', ',', ';', ':', '*', '|', '·', '-']),
                    "",
                )
                .trim()
                .to_owned();

            if !name.contains(' ') {
                continue;
            }

            let key = key(&name);
            let position = *by_key.entry(key).or_insert_with(|| {
                candidates.push(Candidate {
                    field: Field {
                        value: name.clone(),
                        page: line.page,
                        line: text.to_owned(),
                    },
                    score: 0,
                });
                candidates.len() - 1
            });
            candidates[position].score += weight;
        }
    }

    // The highest score; on a tie the first one named.
    candidates
        .into_iter()
        .rev()
        .max_by_key(|candidate| candidate.score)
        .map(|candidate| candidate.field)
}

struct Candidate {
    field: Field,
    score: usize,
}

/// One spelling for `GmbH & Co. KG`, `GmbH + Co KG` and `GmbH und Co. KG`.
fn key(name: &str) -> String {
    name.to_lowercase()
        .replace(['.', ','], "")
        .replace(" und ", " & ")
        .replace('+', "&")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::lines;
    use crate::test_support::{english, german, item};

    fn extract(lines: &[Line]) -> Option<Field> {
        super::extract(lines, german())
    }

    fn seller(items: &[crate::layout::Item]) -> Option<String> {
        extract(&lines(items)).map(|field| field.value)
    }

    #[test]
    fn reads_the_sender_line_above_the_address() {
        assert_eq!(
            seller(&[
                item(
                    1,
                    40.0,
                    800.0,
                    "Beispiel Kellerbau GmbH + Co KG * Industriestr. 3 * 12345 Musterstadt"
                ),
                item(1, 40.0, 780.0, "Erika und Max Mustermann"),
                item(1, 40.0, 770.0, "Musterweg 4"),
            ]),
            Some("Beispiel Kellerbau GmbH + Co KG".to_owned()),
        );
    }

    #[test]
    fn prefers_the_name_repeated_in_every_footer() {
        let mut items = vec![item(
            1,
            40.0,
            800.0,
            "Muster & Partner GmbH, Bahnhofstr.1, 12345 Musterstadt",
        )];
        for page in 1..=5 {
            items.push(item(page, 40.0, 40.0, "Muster & Partner Bau GmbH - Musterstadt"));
        }

        assert_eq!(seller(&items), Some("Muster & Partner Bau GmbH".to_owned()));
    }

    #[test]
    fn finds_the_company_in_its_terms_without_a_sender_line() {
        assert_eq!(
            seller(&[
                item(1, 40.0, 800.0, "Max Mustermann"),
                item(1, 40.0, 790.0, "Musterweg 4"),
                item(
                    3,
                    40.0,
                    500.0,
                    "Ergänzend zu den Lieferbedingungen der Beispielhaus Keller GmbH & Co. KG gilt"
                ),
                item(
                    3,
                    40.0,
                    480.0,
                    "1. Auftragnehmer ist die Beispielhaus Keller GmbH & Co. KG, Am Werk 1"
                ),
                item(
                    3,
                    40.0,
                    470.0,
                    "Es gelten die Zahlungsbedingungen der Beispielhaus Keller GmbH & Co. KG"
                ),
            ]),
            Some("Beispielhaus Keller GmbH & Co. KG".to_owned()),
        );
    }

    #[test]
    fn reads_english_legal_forms_after_a_document_title() {
        let english = |items: &[crate::layout::Item]| super::extract(&lines(items), english()).map(|field| field.value);

        assert_eq!(
            english(&[
                item(1, 40.0, 800.0, "TAX INVOICE Harbourside Fasteners (Pty) Ltd"),
                item(1, 40.0, 780.0, "12 Quay Street, Cape Town"),
            ]),
            Some("Harbourside Fasteners (Pty) Ltd".to_owned()),
        );
        assert_eq!(
            english(&[item(1, 40.0, 800.0, "Riverbend Joinery Limited")]),
            Some("Riverbend Joinery Limited".to_owned()),
        );
        assert_eq!(
            english(&[item(1, 40.0, 800.0, "Copperleaf Analytics Inc")]),
            Some("Copperleaf Analytics Inc".to_owned()),
        );
    }

    #[test]
    fn ends_a_name_at_its_run_when_a_title_sits_beside_it() {
        let lines = lines(&[
            item(1, 40.0, 800.0, "Copperleaf Analytics Inc"),
            item(1, 420.0, 800.0, "INVOICE"),
            item(1, 40.0, 600.0, "Bill To"),
            item(1, 40.0, 585.0, "Harrow & Finch LLP"),
        ]);

        assert_eq!(
            super::extract(&lines, english()).map(|field| field.value),
            Some("Copperleaf Analytics Inc".to_owned())
        );
    }

    #[test]
    fn takes_no_name_under_a_recipient_heading() {
        // The sender has no legal form; the customers named twice do.
        let lines = lines(&[
            item(1, 40.0, 800.0, "Fernhill Bakery"),
            item(1, 420.0, 800.0, "INVOICE"),
            item(1, 40.0, 700.0, "BILL TO"),
            item(1, 300.0, 700.0, "SHIP TO"),
            item(1, 40.0, 685.0, "Harrow & Finch LLP"),
            item(1, 300.0, 685.0, "Harrow & Finch LLP"),
            item(1, 40.0, 600.0, "Customer: Lindqvist Holdings Ltd"),
        ]);

        assert_eq!(super::extract(&lines, english()), None);
    }

    #[test]
    fn names_no_seller_without_a_legal_form() {
        assert_eq!(
            seller(&[
                item(1, 40.0, 800.0, "Hans Maier, Dorfstraße 1, 86150 Augsburg"),
                item(1, 40.0, 780.0, "Rechnung Nr. 4711"),
                item(1, 40.0, 760.0, "Fliesenarbeiten KG Bad"),
            ]),
            None,
        );
    }
}
