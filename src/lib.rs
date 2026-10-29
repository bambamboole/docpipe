//! docpipe analyses incoming documents: it classifies PDFs, extracts their
//! text with positions, finds embedded e-invoice XML and proposes the fields
//! of invoices and quotes with the words of a language profile.
//!
//! The public modules are what the HTTP service, its binary, the integration
//! tests and the fuzz targets use: [`analyze`] runs an analysis and
//! [`candidates`], [`attachments`] and [`ocr`] hold the types of its result.
//! The readers of the layout, numbers, dates, taxes, currencies, sellers and
//! line items are crate-private.

pub mod analyze;
pub mod attachments;
pub mod auth;
pub mod candidates;
pub mod config;
pub(crate) mod currency;
pub(crate) mod dates;
pub mod http;
pub(crate) mod layout;
pub(crate) mod line_items;
pub(crate) mod numbers;
pub mod ocr;
pub(crate) mod profile;
pub(crate) mod seller;
pub(crate) mod tax;
pub mod telemetry;
#[cfg(test)]
pub(crate) mod test_support;

/// A static regex compiled from a literal pattern on first use:
/// `regex!(IBAN, r"\b[A-Z]{2}\d{2}...")`. The pattern is part of the code, so
/// a failure to compile it is a bug, not an error to handle.
macro_rules! regex {
    ($name:ident, $pattern:literal) => {
        static $name: ::std::sync::LazyLock<::regex::Regex> =
            ::std::sync::LazyLock::new(|| ::regex::Regex::new($pattern).expect("valid regex"));
    };
}
pub(crate) use regex;
