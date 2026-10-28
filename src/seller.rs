//! The name of the company that issued a document, read from reconstructed
//! lines.
//!
//! A vendor names itself with its legal form: in the sender line above the
//! address window, in the page footer and in its terms. Every company name
//! with a legal form is a candidate; the one named most often wins, and the
//! first lines of the first page, where the sender line sits, count extra.
//! A private customer has no legal form and is never a candidate.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::candidates::Field;
use crate::layout::Line;

/// Lines at the top of the first page that hold the sender line.
const HEAD_LINES: usize = 8;

/// What a mention in the head counts, against one for any other mention.
const HEAD_WEIGHT: usize = 3;

static COMPANY: LazyLock<Regex> = LazyLock::new(|| {
    // Up to six capitalised words, joined by `&`, `+` or `und`, then the legal
    // form, optionally as a limited partnership (`GmbH & Co. KG`). A bare `KG`,
    // `AG`, `SE` or `OHG` also abbreviates building terms (`KG` is the
    // basement), so it only counts at the end of the name's part of the line.
    Regex::new(
        r"(?x)
        \b\p{Lu}[\p{L}\d.'\-]*
        (?:\s+(?:\p{Lu}[\p{L}\d.'\-]*|&|\+|und)){0,5}
        \s+(?:
            (?:GmbH|mbH|GbR|UG(?:\s\(haftungsbeschränkt\))?|e\.\s?K\.)
            (?:\s*(?:&|\+|und)\s*Co\.?\s*(?:KG|OHG))?
            (?:[\s,;:*|·]|$)
          | (?:KG|AG|SE|OHG)\s*(?:[,;:*|·\-]|$)
        )",
    )
    .expect("valid regex")
});

/// Words that introduce a company in running text rather than name it.
static LEADING_WORDS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:(?:Firma|Fa\.|Die|Der|Den|Von|Bei)\s+)+").expect("valid regex"));

pub fn extract(lines: &[Line]) -> Option<Field> {
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut by_key: HashMap<String, usize> = HashMap::new();

    for (index, line) in lines.iter().enumerate() {
        let text = line.text();
        let weight = if line.page == 1 && index < HEAD_LINES {
            HEAD_WEIGHT
        } else {
            1
        };

        for matched in COMPANY.find_iter(&text) {
            let name = LEADING_WORDS
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
                        line: text.clone(),
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
    use crate::layout::tests::item;

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
