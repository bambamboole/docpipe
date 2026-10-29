//! Language profiles: the words a document is read by.
//!
//! Every profile is a TOML file in `profiles/`, compiled into the binary. It
//! holds the labels of fields, the words that tell document kinds apart, the
//! vocabulary of line items and the legal forms of companies. Everything that
//! does not depend on the language, such as reading the layout or checking
//! IBANs, stays in the code.

use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;

use crate::layout::Line;
use crate::numbers::DecimalMark;

/// The bundled profiles, in the order they are tried.
const SOURCES: &[(&str, &str)] = &[
    ("profiles/de.toml", include_str!("../profiles/de.toml")),
    ("profiles/en.toml", include_str!("../profiles/en.toml")),
];

static PROFILES: LazyLock<Vec<Profile>> = LazyLock::new(|| {
    SOURCES
        .iter()
        .map(|(path, source)| Profile::parse(source).unwrap_or_else(|error| panic!("{path}: {error}")))
        .collect()
});

/// The bundled profiles, parsed on first use; a profile that does not parse
/// panics here, which `cargo test` catches.
#[must_use]
pub fn all() -> &'static [Profile] {
    &PROFILES
}

/// The profile whose labels and document words occur most often on the
/// document; on a tie, the one bundled first.
#[must_use]
pub fn detect(lines: &[Line]) -> &'static Profile {
    all()
        .iter()
        .enumerate()
        .max_by_key(|(index, profile)| (profile.score(lines), std::cmp::Reverse(*index)))
        .map(|(_, profile)| profile)
        .expect("a profile is bundled")
}

/// The words of one language, compiled from its TOML file.
#[derive(Debug)]
pub struct Profile {
    /// A BCP 47 language tag, such as `de`.
    pub id: String,
    /// The decimal mark when a document's amounts do not show it.
    pub decimal_mark: DecimalMark,
    /// The names of the months and the pattern of a written date.
    pub dates: DateWords,
    /// The labels of the fields.
    pub labels: Labels,
    /// The words that tell invoices, quotes and credit notes apart.
    pub document_kind: DocumentKindWords,
    /// The words that tell partial and final invoices apart.
    pub invoice_kind: InvoiceKindWords,
    /// The vocabulary of line items.
    pub line_items: LineItemWords,
    /// The legal forms of companies and the headings of the recipient.
    pub seller: SellerWords,
}

/// The words dates are written with.
#[derive(Debug)]
pub struct DateWords {
    /// A date with a written month: day, month and year in the groups `da`,
    /// `ma`, `ya` (`7. September 2026`), `db`, `mb`, `yb` (`September 7,
    /// 2026`) or `dc`, `mc`, `yc` (`07-Sep-2026`).
    pub written: Regex,
    /// The lowercase names of each month, January first.
    months: Vec<Vec<String>>,
}

impl DateWords {
    /// The number of the month `name` names, January being 1.
    pub fn month(&self, name: &str) -> Option<u32> {
        let name = name.trim_end_matches('.').to_lowercase();

        self.months
            .iter()
            .position(|names| names.contains(&name))
            .map(|index| index as u32 + 1)
    }
}

/// The labels a field's value is looked up after, matched case-insensitively
/// within one run of text.
#[derive(Debug)]
pub struct Labels {
    /// The label of an invoice's number.
    pub invoice_number: Regex,
    /// The label of a quote's number.
    pub quote_number: Regex,
    /// The label of the issue date.
    pub issue_date: Regex,
    /// The label of the due date.
    pub due_date: Regex,
    /// The label of the end of a quote's validity.
    pub valid_until: Regex,
    /// The label of the gross amount.
    pub gross: Regex,
    /// The label of the net amount.
    pub net: Regex,
}

/// The lowercase words that name a kind of document, matched as substrings.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentKindWords {
    /// Words of an invoice.
    pub invoice: Vec<String>,
    /// Words of a quote.
    pub quote: Vec<String>,
    /// Words of a credit note.
    pub credit_note: Vec<String>,
}

/// The lowercase words that name a kind of invoice, matched as substrings on
/// the first page.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvoiceKindWords {
    /// Words of a final invoice.
    pub r#final: Vec<String>,
    /// Words of a partial invoice.
    pub partial: Vec<String>,
}

/// The vocabulary of line items.
#[derive(Debug)]
pub struct LineItemWords {
    /// What a title heading starts with, such as `Titel:`.
    pub title_prefix: String,
    /// A span that stands for a quantity of one, such as `pauschal`.
    pub lump_sum: Regex,
    /// A span on a position that marks it optional, such as `Bedarf`.
    pub optional_marker: Regex,
    /// A section heading whose positions are optional.
    pub optional_section: Regex,
    /// A line ending with a colon that still belongs to a position, such as
    /// `Hinweis:`.
    pub note: Regex,
    /// A line that sums positions, such as `Titelsumme`, and ends them.
    pub total_line: Regex,
}

/// The words the seller is read by.
#[derive(Debug)]
pub struct SellerWords {
    /// A company name ending in a legal form.
    pub company: Regex,
    /// The words before a name that introduce it in running text.
    pub leading_words: Regex,
    /// A run that starts with the heading of the recipient's address.
    pub recipient: Regex,
}

/// A profile as written in its TOML file.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    id: String,
    /// The language's name, for the people who edit the file; the code
    /// knows a profile by its `id`.
    #[expect(dead_code, reason = "documents the file; nothing reads it")]
    name: String,
    numbers: NumberSource,
    dates: DateSource,
    labels: LabelSource,
    document_kind: DocumentKindWords,
    invoice_kind: InvoiceKindWords,
    line_items: LineItemSource,
    seller: SellerSource,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NumberSource {
    decimal_mark: DecimalMark,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DateSource {
    months: Vec<Vec<String>>,
    ordinal_suffixes: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelSource {
    invoice_number: String,
    quote_number: String,
    issue_date: String,
    due_date: String,
    valid_until: String,
    gross: String,
    net: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LineItemSource {
    title_prefix: String,
    lump_sum: String,
    optional_marker: String,
    optional_section: String,
    note: String,
    total_line: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SellerSource {
    legal_forms: Vec<String>,
    partnership_forms: Vec<String>,
    trailing_legal_forms: Vec<String>,
    connectors: Vec<String>,
    leading_words: Vec<String>,
    recipient_headings: Vec<String>,
}

impl Profile {
    /// How many labels and document-kind words of the profile occur on the
    /// document.
    fn score(&self, lines: &[Line]) -> usize {
        let labels = &self.labels;
        let patterns = [
            &labels.invoice_number,
            &labels.quote_number,
            &labels.issue_date,
            &labels.due_date,
            &labels.valid_until,
            &labels.gross,
            &labels.net,
        ];
        let words = &self.document_kind;

        lines
            .iter()
            .map(|line| {
                let text = line.text().to_lowercase();
                let labelled = line
                    .spans()
                    .iter()
                    .map(|span| patterns.iter().filter(|label| label.is_match(&span.text)).count())
                    .sum::<usize>();
                let kind = [&words.invoice, &words.quote, &words.credit_note]
                    .into_iter()
                    .flatten()
                    .filter(|word| text.contains(word.as_str()))
                    .count();

                labelled + kind
            })
            .sum()
    }

    /// The profile of a TOML source. The error is a message with the key at
    /// fault: it is only ever shown, in the panic at startup and in `cargo
    /// test`, so an error type would be more code than value.
    fn parse(source: &str) -> Result<Self, String> {
        let source: Source = toml::from_str(source).map_err(|error| error.to_string())?;
        let labels = source.labels;
        let line_items = source.line_items;

        Ok(Self {
            id: source.id,
            decimal_mark: source.numbers.decimal_mark,
            dates: date_words(&source.dates)?,
            labels: Labels {
                invoice_number: insensitive("labels.invoice_number", &labels.invoice_number)?,
                quote_number: insensitive("labels.quote_number", &labels.quote_number)?,
                issue_date: insensitive("labels.issue_date", &labels.issue_date)?,
                due_date: insensitive("labels.due_date", &labels.due_date)?,
                valid_until: insensitive("labels.valid_until", &labels.valid_until)?,
                gross: insensitive("labels.gross", &labels.gross)?,
                net: insensitive("labels.net", &labels.net)?,
            },
            document_kind: lowercase_words(source.document_kind),
            invoice_kind: InvoiceKindWords {
                r#final: lowercase(source.invoice_kind.r#final),
                partial: lowercase(source.invoice_kind.partial),
            },
            line_items: LineItemWords {
                title_prefix: line_items.title_prefix,
                lump_sum: insensitive("line_items.lump_sum", &line_items.lump_sum)?,
                optional_marker: insensitive("line_items.optional_marker", &line_items.optional_marker)?,
                optional_section: insensitive("line_items.optional_section", &line_items.optional_section)?,
                note: insensitive("line_items.note", &line_items.note)?,
                total_line: insensitive("line_items.total_line", &line_items.total_line)?,
            },
            seller: SellerWords {
                company: compile("seller", &company_pattern(&source.seller))?,
                leading_words: compile(
                    "seller.leading_words",
                    &format!(r"^(?:(?:{})\s+)+", source.seller.leading_words.join("|")),
                )?,
                recipient: insensitive(
                    "seller.recipient_headings",
                    &format!(r"^\s*(?:{})\b", source.seller.recipient_headings.join("|")),
                )?,
            },
        })
    }
}

fn date_words(source: &DateSource) -> Result<DateWords, String> {
    if source.months.len() != 12 || source.months.iter().any(Vec::is_empty) {
        return Err("dates.months: name twelve months, each at least once".to_owned());
    }

    let months: Vec<Vec<String>> = source
        .months
        .iter()
        .map(|names| names.iter().map(|name| name.to_lowercase()).collect())
        .collect();
    let mut names: Vec<&String> = months.iter().flatten().collect();
    names.sort_by_key(|name| std::cmp::Reverse(name.chars().count()));
    let month = format!(
        r"(?:{})\b\.?",
        names
            .iter()
            .map(|name| regex::escape(name))
            .collect::<Vec<_>>()
            .join("|")
    );
    let ordinal = match source.ordinal_suffixes.as_slice() {
        [] => r"\.?".to_owned(),
        suffixes => format!(r"(?:\.|{})?", suffixes.join("|")),
    };
    let pattern = format!(
        r"(?i)\b(?:(?P<da>\d{{1,2}}){ordinal}\s+(?P<ma>{month}),?\s+(?P<ya>\d{{4}})|(?P<mb>{month})\s+(?P<db>\d{{1,2}}){ordinal},?\s+(?P<yb>\d{{4}})|(?P<dc>\d{{1,2}})-(?P<mc>{month})-(?P<yc>\d{{4}}|\d{{2}}))\b"
    );

    Ok(DateWords {
        written: compile("dates", &pattern)?,
        months,
    })
}

/// Up to six capitalised words, joined by a connector, then a legal form,
/// optionally as a partnership (`GmbH & Co. KG`). A trailing legal form only
/// counts at the end of the name's part of the line.
fn company_pattern(seller: &SellerSource) -> String {
    let connectors = seller.connectors.join("|");
    let word = r"\p{Lu}[\p{L}\d.'\-]*";
    let partnership = if seller.partnership_forms.is_empty() {
        String::new()
    } else {
        format!(
            r"(?:\s*(?:{connectors})\s*Co\.?\s*(?:{}))?",
            seller.partnership_forms.join("|")
        )
    };
    let mut forms = vec![format!(
        r"(?:{}){partnership}(?:[\s,;:*|·]|$)",
        seller.legal_forms.join("|")
    )];

    if !seller.trailing_legal_forms.is_empty() {
        forms.push(format!(
            r"(?:{})\s*(?:[,;:*|·\-]|$)",
            seller.trailing_legal_forms.join("|")
        ));
    }

    format!(
        r"\b{word}(?:\s+(?:{word}|{connectors})){{0,5}}\s+(?:{})",
        forms.join("|")
    )
}

fn insensitive(key: &str, pattern: &str) -> Result<Regex, String> {
    compile(key, &format!("(?i){pattern}"))
}

fn compile(key: &str, pattern: &str) -> Result<Regex, String> {
    Regex::new(pattern).map_err(|error| format!("{key}: {error}"))
}

fn lowercase_words(words: DocumentKindWords) -> DocumentKindWords {
    DocumentKindWords {
        invoice: lowercase(words.invoice),
        quote: lowercase(words.quote),
        credit_note: lowercase(words.credit_note),
    }
}

fn lowercase(words: Vec<String>) -> Vec<String> {
    words.into_iter().map(|word| word.to_lowercase()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::lines;
    use crate::test_support::item;

    #[test]
    fn detects_the_language_of_a_document() {
        let german_invoice = lines(&[
            item(1, 50.0, 700.0, "Rechnung"),
            item(1, 50.0, 680.0, "Rechnungsdatum: 07.09.2026"),
            item(1, 50.0, 660.0, "Gesamtbetrag: 119,00 €"),
        ]);
        let english_invoice = lines(&[
            item(1, 50.0, 700.0, "TAX INVOICE"),
            item(1, 50.0, 680.0, "Invoice #: IN0049923"),
            item(1, 50.0, 660.0, "Due Date: 26/09/2025"),
        ]);

        assert_eq!(detect(&german_invoice).id, "de");
        assert_eq!(detect(&english_invoice).id, "en");
        assert_eq!(detect(&[]).id, "de");
    }

    #[test]
    fn parses_every_bundled_profile() {
        assert_eq!(all().len(), SOURCES.len());

        for profile in all() {
            assert!(!profile.document_kind.invoice.is_empty(), "{}", profile.id);
            assert!(!profile.document_kind.quote.is_empty(), "{}", profile.id);
            assert!(!profile.document_kind.credit_note.is_empty(), "{}", profile.id);
        }
    }

    #[test]
    fn gives_every_profile_its_own_id() {
        let mut ids: Vec<&str> = all().iter().map(|profile| profile.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();

        assert_eq!(ids.len(), all().len());
    }

    #[test]
    fn rejects_unknown_keys_and_broken_patterns() {
        let german = include_str!("../profiles/de.toml");

        assert!(Profile::parse(&german.replace("[labels]", "[labels]\ntotal = 'x'")).is_err());
        assert!(
            Profile::parse(&german.replace("net = '", "net = '(")).is_err_and(|error| error.contains("labels.net"))
        );
    }
}
