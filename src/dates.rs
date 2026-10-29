//! Dates as written on a document, read as ISO `YYYY-MM-DD`.
//!
//! ISO dates, dotted dates (`07.09.2026`, day first) and dates with a written
//! month (`7. September 2026`, `Sep 7, 2026`) are unambiguous. A date with
//! slashes or dashes (`03/04/2026`) is day first in some countries and month
//! first in others: a date on the same document whose day exceeds 12 tells
//! which; otherwise it stays ambiguous, and [`settle`] decides by the order of
//! two related dates or not at all.

use regex::Captures;

use crate::layout::Line;
use crate::profile::DateWords;
use crate::regex;

regex!(ISO, r"\b(\d{4})-(\d{1,2})-(\d{1,2})\b");
regex!(DOTTED, r"\b(\d{1,2})\.(\d{1,2})\.(\d{4}|\d{2})\b");
// Dashes need a four-digit year: `11-22-43` is a British sort code.
regex!(
    SLASHED,
    r"\b(?:(\d{1,2})/(\d{1,2})/(\d{4}|\d{2})|(\d{1,2})-(\d{1,2})-(\d{4}))\b"
);

/// The order of day and month in a slashed or dashed date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// `31/12/2026`, as in most of Europe.
    DayFirst,
    /// `12/31/2026`, as in the United States.
    MonthFirst,
}

/// A date as read: certain, or one of two dates when the order of day and
/// month is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    /// The one date the text can mean.
    Date(Date),
    /// A slashed date that is valid in both orders, and differs by them.
    Ambiguous {
        /// The date when the day comes first.
        day_first: Date,
        /// The date when the month comes first.
        month_first: Date,
    },
}

/// A calendar date that exists. Ordered by time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date {
    year: u32,
    month: u32,
    day: u32,
}

impl Date {
    fn new(year: u32, month: u32, day: u32) -> Option<Self> {
        let year = if year < 100 { 2000 + year } else { year };
        let days = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => 29,
            2 => 28,
            _ => return None,
        };

        (1..=days).contains(&day).then_some(Self { year, month, day })
    }

    /// The date as `YYYY-MM-DD`.
    #[must_use]
    pub fn iso(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// Reads the dates of one document.
pub struct DateReader<'p> {
    words: &'p DateWords,
    order: Option<Order>,
}

impl<'p> DateReader<'p> {
    /// The order of slashed dates comes from the unambiguous ones among them;
    /// when they disagree, it stays open.
    pub fn new(lines: &[Line], words: &'p DateWords) -> Self {
        let mut day_first = false;
        let mut month_first = false;

        for line in lines {
            for captures in SLASHED.captures_iter(line.text()) {
                let (first, second, _) = slashed(&captures);
                day_first |= first > 12 && second <= 12;
                month_first |= second > 12 && first <= 12;
            }
        }

        let order = match (day_first, month_first) {
            (true, false) => Some(Order::DayFirst),
            (false, true) => Some(Order::MonthFirst),
            _ => None,
        };

        Self { words, order }
    }

    /// The byte range of the first date in `text`.
    pub fn find(&self, text: &str) -> Option<std::ops::Range<usize>> {
        [&*ISO, &*DOTTED, &*SLASHED, &self.words.written]
            .into_iter()
            .filter_map(|pattern| pattern.find(text))
            .min_by_key(regex::Match::start)
            .map(|found| found.range())
    }

    /// The date `text` starts with, as found by [`DateReader::find`].
    pub fn read(&self, text: &str) -> Option<Reading> {
        let range = self.find(text)?;
        let text = &text[range];

        if let Some(captures) = ISO.captures(text) {
            return Date::new(number(&captures[1]), number(&captures[2]), number(&captures[3])).map(Reading::Date);
        }
        if let Some(captures) = DOTTED.captures(text) {
            return Date::new(number(&captures[3]), number(&captures[2]), number(&captures[1])).map(Reading::Date);
        }
        if let Some(captures) = self.words.written.captures(text) {
            return self.written(&captures).map(Reading::Date);
        }

        let (first, second, year) = slashed(&SLASHED.captures(text)?);
        let day_first = Date::new(year, second, first);
        let month_first = Date::new(year, first, second);

        match (self.order, day_first, month_first) {
            (Some(Order::DayFirst), date, _) | (Some(Order::MonthFirst), _, date) => date.map(Reading::Date),
            (None, Some(day_first), Some(month_first)) if day_first != month_first => {
                Some(Reading::Ambiguous { day_first, month_first })
            }
            (None, Some(date), _) | (None, None, Some(date)) => Some(Reading::Date(date)),
            (None, None, None) => None,
        }
    }

    fn written(&self, captures: &Captures<'_>) -> Option<Date> {
        let group = |names: [&str; 3]| {
            names
                .into_iter()
                .find_map(|name| captures.name(name))
                .map(|found| found.as_str())
        };
        let day = group(["da", "db", "dc"])?;
        let month = group(["ma", "mb", "mc"])?;
        let year = group(["ya", "yb", "yc"])?;

        Date::new(number(year), self.words.month(month)?, number(day))
    }
}

/// The dates of two fields where the second cannot precede the first, such as
/// an issue and a due date. An ambiguous date takes the reading under which
/// the two are in order when only one is; otherwise it stays unread.
pub fn settle(earlier: Option<Reading>, later: Option<Reading>) -> (Option<Date>, Option<Date>) {
    let options = |reading: Option<Reading>| -> Vec<Option<Date>> {
        match reading {
            None => vec![None],
            Some(Reading::Date(date)) => vec![Some(date)],
            Some(Reading::Ambiguous { day_first, month_first }) => vec![Some(day_first), Some(month_first)],
        }
    };
    let certain = |reading: Option<Reading>| match reading {
        Some(Reading::Date(date)) => Some(date),
        _ => None,
    };

    let both = earlier.is_some() && later.is_some();
    if !both {
        return (certain(earlier), certain(later));
    }

    // The same order of day and month applies to both dates of a document.
    let orders = [0, 1].map(|order| {
        let pick = |options: &Vec<Option<Date>>| options[order.min(options.len() - 1)];
        (pick(&options(earlier)), pick(&options(later)))
    });
    let in_order: Vec<(Option<Date>, Option<Date>)> =
        orders.into_iter().filter(|(earlier, later)| earlier <= later).collect();

    match in_order.as_slice() {
        [(earlier, later)] => (*earlier, *later),
        [first, second] if first == second => *first,
        _ => (certain(earlier), certain(later)),
    }
}

/// The first and second number and the year of a slashed or dashed date.
fn slashed(captures: &Captures<'_>) -> (u32, u32, u32) {
    let part = |slashes: usize, dashes: usize| {
        captures
            .get(slashes)
            .or_else(|| captures.get(dashes))
            .map_or(0, |found| number(found.as_str()))
    };

    (part(1, 4), part(2, 5), part(3, 6))
}

fn number(digits: &str) -> u32 {
    digits.parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{german, lines_of};

    fn reader(texts: &[&str]) -> DateReader<'static> {
        DateReader::new(&lines_of(texts), &german().dates)
    }

    fn iso(reader: &DateReader<'_>, text: &str) -> Option<String> {
        match reader.read(text)? {
            Reading::Date(date) => Some(date.iso()),
            Reading::Ambiguous { .. } => Some("ambiguous".to_owned()),
        }
    }

    #[test]
    fn reads_iso_dotted_and_written_dates() {
        let reader = reader(&[]);

        assert_eq!(iso(&reader, "Datum: 2026-09-07"), Some("2026-09-07".to_owned()));
        assert_eq!(iso(&reader, "Datum: 07.09.2026"), Some("2026-09-07".to_owned()));
        assert_eq!(iso(&reader, "7.9.26"), Some("2026-09-07".to_owned()));
        assert_eq!(
            iso(&reader, "Augsburg, den 7. September 2026"),
            Some("2026-09-07".to_owned())
        );
        assert_eq!(iso(&reader, "fällig am 3. März 2026"), Some("2026-03-03".to_owned()));
        assert_eq!(iso(&reader, "vom 12. Okt. 2026"), Some("2026-10-12".to_owned()));
    }

    #[test]
    fn reads_no_date_that_does_not_exist() {
        let reader = reader(&[]);

        assert_eq!(iso(&reader, "31.02.2026"), None);
        assert_eq!(iso(&reader, "29.02.2028"), Some("2028-02-29".to_owned()));
    }

    #[test]
    fn takes_the_order_of_slashed_dates_from_an_unambiguous_one() {
        let british = reader(&["Date: 27/08/2025", "Due Date: 03/09/2025"]);
        let american = reader(&["Date: 03/12/2024", "Due: 04/26/2024"]);

        assert_eq!(iso(&british, "03/09/2025"), Some("2025-09-03".to_owned()));
        assert_eq!(iso(&american, "03/12/2024"), Some("2024-03-12".to_owned()));
    }

    #[test]
    fn reads_no_dashed_date_with_a_two_digit_year() {
        let reader = reader(&[]);

        assert_eq!(iso(&reader, "Sort code: 11-22-43"), None);
        assert_eq!(iso(&reader, "Date: 13-06-2025"), Some("2025-06-13".to_owned()));
    }

    #[test]
    fn leaves_a_slashed_date_ambiguous_without_evidence() {
        let reader = reader(&["Date: 09/03/2026"]);

        assert_eq!(iso(&reader, "09/03/2026"), Some("ambiguous".to_owned()));
        assert_eq!(iso(&reader, "05/05/2026"), Some("2026-05-05".to_owned()));
    }

    #[test]
    fn settles_ambiguous_dates_by_their_order() {
        let reader = reader(&["Date: 09/03/2026", "Due Date: 08/04/2026"]);
        let (issue, due) = settle(reader.read("09/03/2026"), reader.read("08/04/2026"));

        assert_eq!(issue.map(Date::iso), Some("2026-03-09".to_owned()));
        assert_eq!(due.map(Date::iso), Some("2026-04-08".to_owned()));
    }

    #[test]
    fn leaves_dates_unread_when_both_orders_fit() {
        let reader = reader(&["Date: 02/03/2026", "Due Date: 04/05/2026"]);
        let (issue, due) = settle(reader.read("02/03/2026"), reader.read("04/05/2026"));

        assert_eq!((issue, due), (None, None));
    }

    #[test]
    fn keeps_a_lone_ambiguous_date_unread() {
        let reader = reader(&["Date: 02/03/2026"]);

        assert_eq!(settle(reader.read("02/03/2026"), None), (None, None));
    }
}
