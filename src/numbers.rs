//! Amounts and quantities in the number format of a document.
//!
//! A document writes its amounts with a decimal comma (`1.234,56`,
//! `1 234,56`) or a decimal point (`1,234.56`, `1'234.56`). The amounts with
//! two decimals on the document tell which; only when they do not, the
//! profile's mark decides.
//!
//! Read amounts from one span at a time: a space between digit groups is a
//! thousands separator inside a span, but between spans it separates columns,
//! such as a quantity and a price.

use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;

use crate::currency;
use crate::layout::Line;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum DecimalMark {
    #[serde(rename = ",")]
    Comma,
    #[serde(rename = ".")]
    Point,
}

/// The patterns of one decimal mark.
struct Patterns {
    /// An amount with two decimals inside running text.
    amount: Regex,
    /// A span that is only an amount, optionally with a currency before or
    /// after it.
    whole_amount: Regex,
    /// A span that is a quantity and its unit, such as `4,5 m²` or `2 pcs`.
    quantity: Regex,
}

impl Patterns {
    /// `thousands` separates groups of three digits; `decimal` the decimals.
    fn new(thousands: &str, decimal: &str) -> Self {
        let amount = format!(r"-?(?:\d{{1,3}}(?:[{thousands}]\d{{3}})+|\d+){decimal}\d{{2}}\b");
        let number = format!(r"-?\d{{1,3}}(?:[{thousands}]\d{{3}})+(?:{decimal}\d+)?|-?\d+(?:{decimal}\d+)?");
        let currency = currency::affix_pattern();

        Self {
            whole_amount: Regex::new(&format!(r"^(?:{currency})?\s?({amount})\s?(?:{currency})?$"))
                .expect("valid regex"),
            // Not inside a longer number; a letter may precede, as the `R` of
            // `R10 208.07`.
            amount: Regex::new(&format!(r"(?:^|\D)({amount})")).expect("valid regex"),
            quantity: Regex::new(&format!(r"^({number})\s*(\p{{L}}[\p{{L}}²³./]{{0,11}})$")).expect("valid regex"),
        }
    }
}

/// The spaces that separate digit groups.
const GROUP_SPACES: &str = " \u{a0}\u{202f}";

static COMMA: LazyLock<Patterns> = LazyLock::new(|| Patterns::new(&format!(".'{GROUP_SPACES}"), ","));
static POINT: LazyLock<Patterns> = LazyLock::new(|| Patterns::new(&format!(",'{GROUP_SPACES}"), r"\."));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NumberFormat {
    pub mark: DecimalMark,
}

impl NumberFormat {
    pub fn new(mark: DecimalMark) -> Self {
        Self { mark }
    }

    /// The mark that more amounts on the document use; `fallback` on a tie.
    pub fn detect(lines: &[Line], fallback: DecimalMark) -> Self {
        let count = |format: Self| {
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .map(|span| format.amounts(&span.text).count())
                .sum::<usize>()
        };
        let comma = count(Self::new(DecimalMark::Comma));
        let point = count(Self::new(DecimalMark::Point));

        Self::new(match comma.cmp(&point) {
            std::cmp::Ordering::Greater => DecimalMark::Comma,
            std::cmp::Ordering::Less => DecimalMark::Point,
            std::cmp::Ordering::Equal => fallback,
        })
    }

    fn patterns(&self) -> &'static Patterns {
        match self.mark {
            DecimalMark::Comma => &COMMA,
            DecimalMark::Point => &POINT,
        }
    }

    /// The amounts in `text` with their cents. A run of digits that goes on
    /// with another separator, such as the `07.09` of `07.09.2026`, is none.
    pub fn amounts<'t>(&self, text: &'t str) -> impl Iterator<Item = (Range<usize>, i64)> + 't {
        self.patterns()
            .amount
            .captures_iter(text)
            .filter_map(|captures| captures.get(1))
            .filter(|found| standalone(text, found.range()))
            .filter_map(|found| Some((found.range(), cents(found.as_str())?)))
    }

    /// The first amount in `text`.
    pub fn find_amount(&self, text: &str) -> Option<Range<usize>> {
        self.amounts(text).next().map(|(range, _)| range)
    }

    /// The cents of a span that holds only an amount, with or without a
    /// currency.
    pub fn whole_amount(&self, text: &str) -> Option<i64> {
        cents(self.patterns().whole_amount.captures(text.trim())?.get(1)?.as_str())
    }

    /// The quantity, as a decimal with a point, and the unit of a span that
    /// holds only both.
    pub fn quantity(&self, text: &str) -> Option<(String, String)> {
        let captures = self.patterns().quantity.captures(text.trim())?;

        Some((self.decimal(&captures[1]), captures[2].to_owned()))
    }

    /// A number as a decimal with a point and no trailing zeros: `1.234,500`
    /// as `1234.5`.
    fn decimal(&self, number: &str) -> String {
        let decimal_mark = match self.mark {
            DecimalMark::Comma => ',',
            DecimalMark::Point => '.',
        };
        let decimal: String = number
            .chars()
            .filter_map(|c| match c {
                c if c == decimal_mark => Some('.'),
                '0'..='9' | '-' => Some(c),
                _ => None,
            })
            .collect();

        if decimal.contains('.') {
            decimal.trim_end_matches('0').trim_end_matches('.').to_owned()
        } else {
            decimal
        }
    }
}

/// Cents as a decimal with a point and two decimals: `-123456` as `-1234.56`.
pub fn decimal(cents: i64) -> String {
    format!(
        "{}{}.{:02}",
        if cents < 0 { "-" } else { "" },
        cents.abs() / 100,
        cents.abs() % 100
    )
}

/// The cents of an amount with two decimals, whatever its separators.
fn cents(amount: &str) -> Option<i64> {
    amount
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '-')
        .collect::<String>()
        .parse()
        .ok()
}

/// Whether the match is not part of a longer run of digits and separators.
fn standalone(text: &str, range: Range<usize>) -> bool {
    let continues = |mut chars: std::str::Chars<'_>| {
        matches!(chars.next(), Some('.' | ',' | '\'')) && chars.next().is_some_and(|c| c.is_ascii_digit())
    };

    !continues(text[range.end..].chars()) && !continues(text[..range.start].chars().rev().collect::<String>().chars())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::lines;
    use crate::layout::tests::item;

    fn comma() -> NumberFormat {
        NumberFormat::new(DecimalMark::Comma)
    }

    fn point() -> NumberFormat {
        NumberFormat::new(DecimalMark::Point)
    }

    fn cents_in(format: NumberFormat, text: &str) -> Vec<i64> {
        format.amounts(text).map(|(_, cents)| cents).collect()
    }

    #[test]
    fn reads_amounts_with_either_decimal_mark() {
        assert_eq!(cents_in(comma(), "Summe 1.234,56 € und 12,50"), [123456, 1250]);
        assert_eq!(cents_in(point(), "Total $1,234.56 and 12.50"), [123456, 1250]);
        assert_eq!(cents_in(point(), "CHF 1'234.50"), [123450]);
        assert_eq!(cents_in(comma(), "8\u{202f}672,10 €"), [867210]);
        assert_eq!(cents_in(point(), "TOTAL NAD 53 625.87"), [5362587]);
        assert_eq!(cents_in(point(), "Subtotal R10 208.07"), [1020807]);
        assert_eq!(cents_in(comma(), "Gutschrift -50,00"), [-5000]);
    }

    #[test]
    fn reads_no_amount_in_dates_or_plain_spaced_digits() {
        assert!(cents_in(point(), "Date: 07.09.2026").is_empty());
        assert!(cents_in(comma(), "Datum: 2026,07,09").is_empty());
    }

    #[test]
    fn detects_the_mark_most_amounts_use() {
        let us = lines(&[
            item(1, 50.0, 700.0, "Subtotal $346.50"),
            item(1, 50.0, 680.0, "Sales Tax (7.5%) $25.99"),
            item(1, 50.0, 660.0, "Date: 03.12.2024"),
        ]);
        let french = lines(&[
            item(1, 50.0, 700.0, "Subtotal € 8.672,10"),
            item(1, 50.0, 680.0, "TVA 20% € 1.734,42"),
        ]);

        assert_eq!(NumberFormat::detect(&us, DecimalMark::Comma), point());
        assert_eq!(NumberFormat::detect(&french, DecimalMark::Point), comma());
        assert_eq!(NumberFormat::detect(&lines(&[]), DecimalMark::Comma), comma());
    }

    #[test]
    fn reads_a_span_that_is_only_an_amount_with_its_currency() {
        assert_eq!(comma().whole_amount("1.500,00"), Some(150000));
        assert_eq!(comma().whole_amount("662,72 €"), Some(66272));
        assert_eq!(comma().whole_amount("€ 8.672,10"), Some(867210));
        assert_eq!(point().whole_amount("$2,386.40"), Some(238640));
        assert_eq!(point().whole_amount("£18,011.58"), Some(1801158));
        assert_eq!(point().whole_amount("2,386.40 USD"), Some(238640));
        assert_eq!(point().whole_amount("Total 2,386.40"), None);
    }

    #[test]
    fn reads_quantities_with_their_unit() {
        assert_eq!(
            comma().quantity("4,000 Wochen"),
            Some(("4".to_owned(), "Wochen".to_owned()))
        );
        assert_eq!(
            comma().quantity("82,45 m²"),
            Some(("82.45".to_owned(), "m²".to_owned()))
        );
        assert_eq!(
            comma().quantity("1.250 Stk"),
            Some(("1250".to_owned(), "Stk".to_owned()))
        );
        assert_eq!(
            point().quantity("1,250.5 pcs"),
            Some(("1250.5".to_owned(), "pcs".to_owned()))
        );
        assert_eq!(point().quantity("2 hrs"), Some(("2".to_owned(), "hrs".to_owned())));
    }
}
