# Changelog

## [0.2.0](https://github.com/bambamboole/docpipe/compare/v0.1.0...v0.2.0) (2026-10-29)


### ⚠ BREAKING CHANGES

* `candidates.profile` is new and documents are no longer always read with the German profile.
* `amounts.tax_rate` is replaced by `amounts.tax_lines` (`rate`, `net`, `tax`, rate as a decimal string). Line items gain `tax_rate`, set when a percentage on the position is a confirmed rate.

### Features

* analyse German invoices and quotes as a stateless service ([f38104c](https://github.com/bambamboole/docpipe/commit/f38104c829f308f2de0ed0885e2095bde9b978f2))
* describe the API as OpenAPI 3.1 and report to Sentry ([#7](https://github.com/bambamboole/docpipe/issues/7)) ([df51bb7](https://github.com/bambamboole/docpipe/commit/df51bb7b34b0c354da9709d7bd572bb193c37c19))
* read English documents and detect the profile per document ([#11](https://github.com/bambamboole/docpipe/issues/11)) ([0aff9aa](https://github.com/bambamboole/docpipe/commit/0aff9aaf9c9f021f9be823cb64f1e239f0f946fe))
* read numbers, dates, currencies and VAT numbers in any format ([#9](https://github.com/bambamboole/docpipe/issues/9)) ([97b5366](https://github.com/bambamboole/docpipe/commit/97b536685abcb0e7d6ebf22735814a2bb809fd74))
* read tax lines from the rates a document prints ([#10](https://github.com/bambamboole/docpipe/issues/10)) ([c240539](https://github.com/bambamboole/docpipe/commit/c2405394332669e7318c56a1ca91484d9945184d))
* serve an API reference at /docs and release 0.2.0 ([#12](https://github.com/bambamboole/docpipe/issues/12)) ([087a0eb](https://github.com/bambamboole/docpipe/commit/087a0eb683df6311ebf4959513db5c8befc2b9ba))


### Refactors

* harden the service against load and hostile input, tighten lints and docs ([#18](https://github.com/bambamboole/docpipe/issues/18)) ([0d09dd2](https://github.com/bambamboole/docpipe/commit/0d09dd2d60c49ac4d860c082b590296443e5a239))
* read the words of German documents from a language profile ([#8](https://github.com/bambamboole/docpipe/issues/8)) ([825931d](https://github.com/bambamboole/docpipe/commit/825931d92d01bdf051a4da4efdeb29269f813dc5))


### Build

* **deps:** bump the dependencies dependabot proposed ([#13](https://github.com/bambamboole/docpipe/issues/13)) ([89945bf](https://github.com/bambamboole/docpipe/commit/89945bffed98177a137451f161f61bc7c3828c6c))
