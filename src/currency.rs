//! The currency of a document, from the codes and symbols written next to its
//! numbers.
//!
//! An ISO 4217 code names its currency; so do `€`, `£` and prefixed dollar
//! signs such as `US$`. A bare `$` stands for several currencies and names
//! none. A document that names more than one currency, as an invoice with an
//! exchange rate does, has none: which one it is payable in is not certain.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;

use crate::layout::Line;

/// The codes recognised next to numbers.
const CODES: &[&str] = &[
    "EUR", "USD", "GBP", "CHF", "CAD", "AUD", "NZD", "JPY", "CNY", "SEK", "NOK", "DKK", "ISK", "PLN", "CZK", "HUF",
    "RON", "BGN", "TRY", "INR", "ZAR", "NAD", "BWP", "SGD", "HKD", "MXN", "BRL",
];

/// Symbols that stand for exactly one currency.
const SYMBOLS: &[(&str, &str)] = &[
    ("US$", "USD"),
    ("CA$", "CAD"),
    ("AU$", "AUD"),
    ("NZ$", "NZD"),
    ("N$", "NAD"),
    ("C$", "CAD"),
    ("A$", "AUD"),
    ("€", "EUR"),
    ("£", "GBP"),
];

/// A code or unambiguous symbol directly before or after a digit.
static NAMED: LazyLock<Regex> = LazyLock::new(|| {
    let mut tokens: Vec<&str> = SYMBOLS
        .iter()
        .map(|(symbol, _)| *symbol)
        .chain(CODES.iter().copied())
        .collect();
    tokens.sort_by_key(|token| std::cmp::Reverse(token.len()));
    let tokens = tokens
        .iter()
        .map(|token| regex::escape(token))
        .collect::<Vec<_>>()
        .join("|");

    Regex::new(&format!(
        r"(?:^|[^\p{{L}}\p{{N}}$])(?P<before>{tokens})\s?-?\d|\d\s?(?P<after>{tokens})(?:[^\p{{L}}\p{{N}}]|$)"
    ))
    .expect("valid regex")
});

/// A pattern for anything that may stand before or after an amount as its
/// currency, including the ambiguous `$` and the rand's `R`.
pub fn affix_pattern() -> String {
    format!(r"€|£|[A-Z]{{0,2}}\$|R|{}", CODES.join("|"))
}

/// The one currency the document names next to its numbers, if it names
/// exactly one.
pub fn detect(lines: &[Line]) -> Option<String> {
    match named(lines).into_iter().collect::<Vec<_>>().as_slice() {
        [only] => Some((*only).to_owned()),
        _ => None,
    }
}

/// Whether the document names more than one currency next to its numbers.
pub fn several(lines: &[Line]) -> bool {
    named(lines).len() > 1
}

fn named(lines: &[Line]) -> BTreeSet<&'static str> {
    lines
        .iter()
        .flat_map(|line| {
            let text = line.text();

            NAMED
                .captures_iter(&text)
                .filter_map(|captures| {
                    let token = captures.name("before").or_else(|| captures.name("after"))?.as_str();
                    currency(token)
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn currency(token: &str) -> Option<&'static str> {
    SYMBOLS
        .iter()
        .find(|(symbol, _)| *symbol == token)
        .map(|(_, code)| *code)
        .or_else(|| CODES.iter().find(|code| **code == token).copied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::lines;
    use crate::layout::tests::item;

    fn currency(texts: &[&str]) -> Option<String> {
        let items: Vec<_> = texts
            .iter()
            .enumerate()
            .map(|(index, text)| item(1, 50.0, 700.0 - index as f32 * 20.0, text))
            .collect();

        detect(&lines(&items))
    }

    #[test]
    fn reads_codes_and_unambiguous_symbols_next_to_numbers() {
        assert_eq!(currency(&["Summe 41.018,40 €"]), Some("EUR".to_owned()));
        assert_eq!(currency(&["Subtotal € 8.672,10"]), Some("EUR".to_owned()));
        assert_eq!(currency(&["Subtotal £18,011.58"]), Some("GBP".to_owned()));
        assert_eq!(currency(&["Total 1,200.00 CHF"]), Some("CHF".to_owned()));
        assert_eq!(currency(&["Total US$1,200.00"]), Some("USD".to_owned()));
        assert_eq!(currency(&["TOTAL NAD 53 625.87"]), Some("NAD".to_owned()));
    }

    #[test]
    fn names_no_currency_for_a_bare_dollar_sign() {
        assert_eq!(currency(&["Subtotal $346.50", "TOTAL $372.49"]), None);
    }

    #[test]
    fn names_none_when_a_document_names_two_currencies() {
        // An invoice in dollars that converts its total into rand.
        assert_eq!(
            currency(&["TOTAL USD US$ 179.62", "Exchange rate: 1 USD = 18.4512 ZAR"]),
            None
        );
    }

    #[test]
    fn ignores_codes_inside_words_and_away_from_numbers() {
        assert_eq!(currency(&["ITEMEUR 100.00"]), None);
        assert_eq!(currency(&["100.00 EURO"]), None);
        assert_eq!(
            currency(&["CAD drawing set", "Total 1.234,00 €"]),
            Some("EUR".to_owned())
        );
    }
}
