//! Taxes as printed: the rates a document names and the amounts they apply to.
//!
//! A rate counts only when the document prints it (`19 %`, `7,5%`,
//! `VAT 20%`) and an amount on the document is that rate of another one; no
//! rate is assumed from a country. The tax lines of a document are the net
//! and tax amounts per rate that add up to a gross amount it prints.

use std::collections::BTreeSet;

use crate::layout::Line;
use crate::regex;

// A percentage of up to two digits with up to three decimals, not part of a
// longer number; and a span that is only one.
regex!(PERCENT, r"(?:^|[^\d.,'])(\d{1,2})(?:[.,](\d{1,3}))?\s?%");
regex!(ONLY_PERCENT, r"^(\d{1,2})(?:[.,](\d{1,3}))?\s?%$");

/// How many combinations of tax lines are tried before the search gives up
/// and leaves the totals unread.
const SEARCH_LIMIT: usize = 100_000;

/// A tax rate in thousandths of a percent: `19000` for 19 %.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rate(i64);

impl Rate {
    fn parse(whole: &str, fraction: Option<&str>) -> Option<Self> {
        let whole: i64 = whole.parse().ok()?;
        let fraction: i64 = match fraction {
            Some(digits) => format!("{digits:0<3}").parse().ok()?,
            None => 0,
        };
        let rate = whole * 1_000 + fraction;

        (rate > 0).then_some(Self(rate))
    }

    /// The rate of a span that is only a percentage, such as `19 %`.
    pub fn of_span(text: &str) -> Option<Self> {
        let captures = ONLY_PERCENT.captures(text.trim())?;

        Self::parse(&captures[1], captures.get(2).map(|found| found.as_str()))
    }

    /// The tax on `net` cents, rounded to a cent.
    fn tax_on(self, net: i64) -> i64 {
        (net * self.0 + 50_000).div_euclid(100_000)
    }

    /// The rate in percent as a decimal with a point and no trailing zeros:
    /// `19`, `7.5`, `8.625`.
    #[must_use]
    pub fn decimal(self) -> String {
        let text = format!("{}.{:03}", self.0 / 1_000, self.0 % 1_000);

        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    }
}

/// The distinct rates printed anywhere on the document.
pub fn rates(lines: &[Line]) -> Vec<Rate> {
    let rates: BTreeSet<Rate> = lines
        .iter()
        .flat_map(|line| {
            PERCENT
                .captures_iter(line.text())
                .filter_map(|captures| Rate::parse(&captures[1], captures.get(2).map(|found| found.as_str())))
                .collect::<Vec<_>>()
        })
        .collect();

    rates.into_iter().collect()
}

/// The net and tax amount, in cents, of one rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaxLine {
    /// The rate the net amount is taxed at.
    pub rate: Rate,
    /// The net amount in cents.
    pub net: i64,
    /// The tax on `net` in cents, as printed.
    pub tax: i64,
}

/// Totals whose tax lines add up to a gross amount on the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breakdown {
    /// The sum of the lines' net amounts, in cents.
    pub net: i64,
    /// The sum of the lines' tax amounts, in cents.
    pub tax: i64,
    /// `net` plus `tax`, an amount printed on the document.
    pub gross: i64,
    /// One line per rate, ordered by rate.
    pub lines: Vec<TaxLine>,
}

/// The largest gross amount among `amounts` that is the sum of tax lines at
/// the printed `rates`, each line's net and tax being amounts on the document
/// too. With more than one rate, the total net or the total tax must be
/// printed as well, which keeps chance sums out.
pub fn breakdown(amounts: &[i64], rates: &[Rate]) -> Option<Breakdown> {
    let amounts: BTreeSet<i64> = amounts.iter().copied().filter(|cents| *cents > 0).collect();
    let candidates: Vec<Vec<TaxLine>> = rates
        .iter()
        .map(|&rate| {
            amounts
                .iter()
                .filter_map(|&net| {
                    let tax = rate.tax_on(net);
                    // Rounded per position, the tax may be a cent off.
                    [tax, tax - 1, tax + 1]
                        .into_iter()
                        .find(|tax| *tax > 0 && amounts.contains(tax))
                        .map(|tax| TaxLine { rate, net, tax })
                })
                .collect()
        })
        .collect();

    let mut search = Search {
        amounts: &amounts,
        candidates: &candidates,
        chosen: Vec::new(),
        best: None,
        tried: 0,
    };
    search.from(0);

    if search.tried > SEARCH_LIMIT {
        return None;
    }

    search.best
}

struct Search<'a> {
    amounts: &'a BTreeSet<i64>,
    /// The possible tax lines of each rate.
    candidates: &'a [Vec<TaxLine>],
    chosen: Vec<TaxLine>,
    best: Option<Breakdown>,
    tried: usize,
}

impl Search<'_> {
    /// Tries every choice of at most one tax line per rate, from the rate at
    /// `index` on.
    fn from(&mut self, index: usize) {
        if self.tried > SEARCH_LIMIT {
            return;
        }
        let Some(lines) = self.candidates.get(index) else {
            self.tried += 1;
            self.consider();
            return;
        };

        self.from(index + 1);
        for line in lines {
            self.chosen.push(*line);
            self.from(index + 1);
            self.chosen.pop();
        }
    }

    fn consider(&mut self) {
        if self.chosen.is_empty() {
            return;
        }

        let net: i64 = self.chosen.iter().map(|line| line.net).sum();
        let tax: i64 = self.chosen.iter().map(|line| line.tax).sum();
        let gross = net + tax;
        let printed = self.amounts.contains(&gross)
            && (self.chosen.len() == 1 || self.amounts.contains(&net) || self.amounts.contains(&tax));
        let better = self
            .best
            .as_ref()
            .is_none_or(|best| (gross, self.chosen.len()) > (best.gross, best.lines.len()));

        if printed && better {
            self.best = Some(Breakdown {
                net,
                tax,
                gross,
                lines: self.chosen.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::lines_of;

    fn rates_in(texts: &[&str]) -> Vec<String> {
        rates(&lines_of(texts)).into_iter().map(Rate::decimal).collect()
    }

    fn rate(percent: &str) -> Rate {
        Rate::of_span(percent).expect("a rate")
    }

    #[test]
    fn reads_the_rates_a_document_prints() {
        assert_eq!(
            rates_in(&[
                "MwSt. 19,00 %:",
                "Sales Tax (7.5%)",
                "TVA 20%",
                "MWST 8.1 %",
                "Tax (8.625%)"
            ]),
            ["7.5", "8.1", "8.625", "19", "20"]
        );
        assert!(rates_in(&["100%", "1.234,5%", "Rechnung 4711"]).is_empty());
    }

    #[test]
    fn reads_a_span_that_is_only_a_rate() {
        assert_eq!(Rate::of_span("19 %").map(Rate::decimal), Some("19".to_owned()));
        assert_eq!(Rate::of_span("7,00%").map(Rate::decimal), Some("7".to_owned()));
        assert_eq!(Rate::of_span("MwSt. 19 %"), None);
    }

    #[test]
    fn finds_one_tax_line_at_a_printed_rate() {
        let found = breakdown(&[325_000, 61_750, 386_750, 1_000], &[rate("19%")]);

        assert_eq!(
            found,
            Some(Breakdown {
                net: 325_000,
                tax: 61_750,
                gross: 386_750,
                lines: vec![TaxLine {
                    rate: rate("19%"),
                    net: 325_000,
                    tax: 61_750
                }],
            })
        );
    }

    #[test]
    fn finds_no_tax_line_at_a_rate_the_document_does_not_print() {
        assert_eq!(breakdown(&[325_000, 61_750, 386_750], &[rate("7%")]), None);
        assert_eq!(breakdown(&[325_000, 61_750, 386_750], &[]), None);
    }

    #[test]
    fn splits_totals_into_a_line_per_rate() {
        // Net 100,00 at 19 % and 50,00 at 7 %, net total 150,00, gross 172,50.
        let found =
            breakdown(&[10_000, 1_900, 5_000, 350, 15_000, 17_250], &[rate("7%"), rate("19%")]).expect("a breakdown");

        assert_eq!((found.net, found.tax, found.gross), (15_000, 2_250, 17_250));
        assert_eq!(
            found
                .lines
                .iter()
                .map(|line| (line.rate.decimal(), line.net, line.tax))
                .collect::<Vec<_>>(),
            [("7".to_owned(), 5_000, 350), ("19".to_owned(), 10_000, 1_900)]
        );
    }

    #[test]
    fn needs_a_printed_total_net_or_tax_for_several_rates() {
        assert_eq!(
            breakdown(&[10_000, 1_900, 5_000, 350, 17_250], &[rate("7%"), rate("19%")]),
            None
        );
    }
}
