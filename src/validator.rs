//! Lightweight CQL syntax validator for EPO Open Patent Services.
//!
//! Catches the common mistakes that produce silent EPO 400s — unbalanced
//! parens/quotes, missing field-code values, malformed proximity
//! operators, unknown field codes — before the request leaves your
//! process.
//!
//! Typical uses: pre-flighting LLM-generated CQL queries (so a
//! malformed query surfaces as a typed error the LLM can correct on
//! the next round), and rendering per-query validation badges in
//! user-facing UIs.
//!
//! Not a full grammar parser. Pragmatic checks: structural balance +
//! known field codes + operator placement. False negatives are
//! acceptable (EPO will still reject); false positives must be
//! avoided since they'd block legitimate queries.

use std::fmt;

/// Recognised EPO CQL field codes. Source: EPO OPS docs (search field
/// reference). Comparators (`=`, `>=`, `<=`, `>`, `<`, `within`) all attach
/// to these prefixes — we accept any code in this set followed by one of
/// the comparators. `within` is the EPO-recommended form for date ranges
/// (`pd within "20240101,20240107"`); EPO returns a `CLIENT.FuzzyDateRanges`
/// fault on `pd>=… AND pd<=…`, so the validator surfaces both forms.
pub const FIELD_CODES: &[&str] = &[
    "ti", "ab", "ta", "cl", "ic", "cpc", "pa", "pd", "ct", "in", "pn", "ap", "pr", "num", "txt",
];

/// Reserved Boolean operator keywords. Matched case-insensitively.
const BOOLEAN_OPS: &[&str] = &["AND", "OR", "NOT"];

const WITHIN_OP: &str = "within";

/// CQL validation errors. `position` is the 0-based char offset into
/// the input where the problem was first detected.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CqlError {
    pub position: usize,
    pub kind: CqlErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CqlErrorKind {
    Empty,
    UnbalancedParen,
    UnbalancedQuote,
    UnknownFieldCode(String),
    EmptyFieldValue(String),
    BadProximity(String),
    DanglingOperator(String),
    LeadingOperator(String),
}

impl fmt::Display for CqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            CqlErrorKind::Empty => write!(f, "query is empty"),
            CqlErrorKind::UnbalancedParen => {
                write!(f, "unbalanced parenthesis at position {}", self.position)
            }
            CqlErrorKind::UnbalancedQuote => {
                write!(f, "unbalanced quote at position {}", self.position)
            }
            CqlErrorKind::UnknownFieldCode(c) => {
                write!(
                    f,
                    "unknown field code `{}` at position {}",
                    c, self.position
                )
            }
            CqlErrorKind::EmptyFieldValue(c) => write!(
                f,
                "field code `{}` has no value at position {}",
                c, self.position
            ),
            CqlErrorKind::BadProximity(s) => write!(
                f,
                "malformed proximity operator `{}` at position {}",
                s, self.position
            ),
            CqlErrorKind::DanglingOperator(op) => write!(
                f,
                "Boolean operator `{}` at end of query (position {})",
                op, self.position
            ),
            CqlErrorKind::LeadingOperator(op) => write!(
                f,
                "Boolean operator `{}` at start of query (position {})",
                op, self.position
            ),
        }
    }
}

impl std::error::Error for CqlError {}

/// Successful validation result. `query` is the trimmed input; field
/// chips are an extracted list of `(code, value)` pairs the panel
/// renders next to the CQL — driven off the same parse so the badge
/// can't drift from the chip list.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ValidatedCql {
    pub query: String,
    pub fields: Vec<(String, String)>,
}

/// Validate a CQL string. Returns the parsed-fields decomposition on
/// success or the first error encountered.
pub fn validate(input: &str) -> Result<ValidatedCql, CqlError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(CqlError {
            position: 0,
            kind: CqlErrorKind::Empty,
        });
    }

    // Pass 1: structural — parens + quotes balanced; capture quoted
    // ranges so passes 2 + 3 can skip operator/field detection inside
    // string literals.
    let bytes = trimmed.as_bytes();
    let mut paren_depth = 0i32;
    let mut paren_open_at = 0usize;
    let mut in_quote = false;
    let mut quote_open_at = 0usize;
    let mut quoted_ranges: Vec<(usize, usize)> = Vec::new();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' => {
                if in_quote {
                    quoted_ranges.push((quote_open_at, i));
                    in_quote = false;
                } else {
                    quote_open_at = i;
                    in_quote = true;
                }
            }
            b'(' if !in_quote => {
                if paren_depth == 0 {
                    paren_open_at = i;
                }
                paren_depth += 1;
            }
            b')' if !in_quote => {
                paren_depth -= 1;
                if paren_depth < 0 {
                    return Err(CqlError {
                        position: i,
                        kind: CqlErrorKind::UnbalancedParen,
                    });
                }
            }
            _ => {}
        }
    }
    if in_quote {
        return Err(CqlError {
            position: quote_open_at,
            kind: CqlErrorKind::UnbalancedQuote,
        });
    }
    if paren_depth != 0 {
        return Err(CqlError {
            position: paren_open_at,
            kind: CqlErrorKind::UnbalancedParen,
        });
    }

    let in_quote_at = |idx: usize| quoted_ranges.iter().any(|&(s, e)| idx >= s && idx <= e);

    // Pass 2: field codes — every `code=` / `code>=` / `code<=` / `code>` /
    // `code<` outside quotes must be a known field, and have a non-empty
    // value before the next operator/paren.
    let mut fields: Vec<(String, String)> = Vec::new();
    let chars: Vec<char> = trimmed.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_alphabetic() || in_quote_at(i) {
            i += 1;
            continue;
        }
        // Walk an alpha-only run.
        let start = i;
        while i < chars.len() && chars[i].is_ascii_alphabetic() {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        // Skip past optional whitespace before `=` so `ti =foo` parses
        // identically to `ti=foo`.
        let mut j = i;
        while j < chars.len() && chars[j] == ' ' {
            j += 1;
        }
        let comparator_kind = detect_comparator(&chars, j);
        let Some((op, after_op)) = comparator_kind else {
            // Bare word — not a field code, just a search term.
            continue;
        };
        let mut k = after_op;
        // `within` requires whitespace before its value (`pd within "X,Y"`).
        // The other comparators do NOT — `ta= AND …` must surface as
        // EmptyFieldValue, which relies on the value-scan starting AT the
        // space so the AND boundary check fires immediately.
        if op == "within" {
            while k < chars.len() && chars[k] == ' ' {
                k += 1;
            }
        }
        // Validate the field code.
        let lower = word.to_lowercase();
        if !FIELD_CODES.contains(&lower.as_str()) {
            return Err(CqlError {
                position: start,
                kind: CqlErrorKind::UnknownFieldCode(word),
            });
        }
        // Capture the value: read until the next top-level Boolean
        // operator boundary or matching paren close. Empty-value catch:
        // if k points at end-of-input or a closing paren / op-keyword
        // without seeing a non-space char, that's a bug.
        let value_start = k;
        let mut value_end = k;
        let mut local_paren = 0i32;
        let mut local_quote = false;
        while value_end < chars.len() {
            let c = chars[value_end];
            if local_quote {
                if c == '"' {
                    local_quote = false;
                }
            } else if c == '"' {
                local_quote = true;
            } else if c == '(' {
                local_paren += 1;
            } else if c == ')' {
                if local_paren == 0 {
                    break;
                }
                local_paren -= 1;
            } else if local_paren == 0
                && (c == ' ' || c == '\t')
                && peek_boolean_op(&chars, value_end + 1)
            {
                break;
            }
            value_end += 1;
        }
        let value: String = chars[value_start..value_end].iter().collect();
        let trimmed_val = value.trim();
        // Strip surrounding double quotes (e.g. `within "X,Y"` or `pa="Foo"`).
        // Only when both ends match — a lone leading `"` is left alone so a
        // malformed value still surfaces as itself rather than silently mutated.
        let cleaned = trimmed_val
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(trimmed_val);
        if cleaned.is_empty() {
            return Err(CqlError {
                position: start,
                kind: CqlErrorKind::EmptyFieldValue(word),
            });
        }
        fields.push((lower, cleaned.to_string()));
        i = value_end;
    }

    // Pass 3: operator placement — a Boolean op at very start or very
    // end (after stripping whitespace + parens) is malformed. Catches
    // dangling AND/OR/NOT from copy-paste truncation.
    let token_lc = trimmed.to_uppercase();
    for op in BOOLEAN_OPS {
        if token_lc.starts_with(&format!("{op} "))
            || token_lc.starts_with(&format!("{op}("))
            || token_lc.starts_with(&format!("({op} "))
        {
            return Err(CqlError {
                position: 0,
                kind: CqlErrorKind::LeadingOperator((*op).to_string()),
            });
        }
        if token_lc.ends_with(&format!(" {op}")) || token_lc.ends_with(&format!(" {op})")) {
            return Err(CqlError {
                position: trimmed.len() - op.len(),
                kind: CqlErrorKind::DanglingOperator((*op).to_string()),
            });
        }
    }

    // Pass 4: proximity — `NEAR/n` or `near/n` must have a positive
    // integer after the slash.
    let lower = trimmed.to_ascii_lowercase();
    let mut search_from = 0usize;
    while let Some(rel) = lower[search_from..].find("near/") {
        let abs = search_from + rel;
        let after = abs + "near/".len();
        let mut end = after;
        while end < lower.len() && lower.as_bytes()[end].is_ascii_digit() {
            end += 1;
        }
        if end == after {
            return Err(CqlError {
                position: abs,
                kind: CqlErrorKind::BadProximity(format!(
                    "{}/?",
                    &trimmed[abs..abs + "near".len()]
                )),
            });
        }
        search_from = end;
    }

    Ok(ValidatedCql {
        query: trimmed.to_string(),
        fields,
    })
}

/// At offset `idx`, recognise an EPO comparator and return its end offset.
/// Handles `=`, `>=`, `<=`, `>`, `<`, and the case-insensitive `within`
/// keyword used for date ranges.
fn detect_comparator(chars: &[char], idx: usize) -> Option<(&'static str, usize)> {
    if idx >= chars.len() {
        return None;
    }
    match chars[idx] {
        '=' => Some(("=", idx + 1)),
        '>' | '<' => {
            let op = if chars[idx] == '>' { ">" } else { "<" };
            let next = idx + 1;
            if next < chars.len() && chars[next] == '=' {
                Some((if op == ">" { ">=" } else { "<=" }, next + 1))
            } else {
                Some((op, next))
            }
        }
        _ => {
            // Try `within` (case-insensitive). Must be followed by a
            // boundary char (whitespace) or end-of-input — otherwise it's
            // a longer alpha word that happens to start with `within`.
            let len = WITHIN_OP.len();
            if idx + len > chars.len() {
                return None;
            }
            let slice: String = chars[idx..idx + len].iter().collect();
            if !slice.eq_ignore_ascii_case(WITHIN_OP) {
                return None;
            }
            let after = idx + len;
            let bounded = after == chars.len()
                || chars[after] == ' '
                || chars[after] == '\t'
                || chars[after] == '"';
            if bounded {
                Some(("within", after))
            } else {
                None
            }
        }
    }
}

/// Lookahead helper: at offset `idx`, do the next 2-3 chars spell one
/// of AND/OR/NOT followed by a boundary char? Used to terminate
/// field-value extraction without consuming the operator.
fn peek_boolean_op(chars: &[char], idx: usize) -> bool {
    let rest: String = chars.iter().skip(idx).take(4).collect();
    let upper = rest.to_uppercase();
    BOOLEAN_OPS.iter().any(|op| {
        let len = op.len();
        upper.len() >= len
            && upper.starts_with(op)
            && (upper.len() == len
                || upper.as_bytes()[len] == b' '
                || upper.as_bytes()[len] == b'(')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_is_rejected() {
        let err = validate("").unwrap_err();
        assert_eq!(err.kind, CqlErrorKind::Empty);
        let err = validate("    ").unwrap_err();
        assert_eq!(err.kind, CqlErrorKind::Empty);
    }

    #[test]
    fn balanced_parens_pass() {
        validate("ta=(\"solar\" OR \"photovoltaic\") AND cpc=H02S").unwrap();
    }

    #[test]
    fn unbalanced_open_paren_fails() {
        let err = validate("ta=(solar AND cpc=H02S").unwrap_err();
        assert_eq!(err.kind, CqlErrorKind::UnbalancedParen);
    }

    #[test]
    fn unbalanced_close_paren_fails() {
        let err = validate("ta=solar) AND cpc=H02S").unwrap_err();
        assert_eq!(err.kind, CqlErrorKind::UnbalancedParen);
    }

    #[test]
    fn unbalanced_quote_fails() {
        let err = validate("ta=\"solar AND cpc=H02S").unwrap_err();
        assert_eq!(err.kind, CqlErrorKind::UnbalancedQuote);
    }

    #[test]
    fn unknown_field_code_fails() {
        let err = validate("foo=solar").unwrap_err();
        assert!(matches!(err.kind, CqlErrorKind::UnknownFieldCode(_)));
    }

    #[test]
    fn empty_field_value_fails() {
        let err = validate("ta= AND cpc=H02S").unwrap_err();
        assert!(matches!(err.kind, CqlErrorKind::EmptyFieldValue(_)));
    }

    #[test]
    fn dangling_operator_fails() {
        let err = validate("ta=solar AND").unwrap_err();
        assert!(matches!(err.kind, CqlErrorKind::DanglingOperator(_)));
    }

    #[test]
    fn leading_operator_fails() {
        let err = validate("AND ta=solar").unwrap_err();
        assert!(matches!(err.kind, CqlErrorKind::LeadingOperator(_)));
    }

    #[test]
    fn proximity_with_n_passes() {
        validate("ta=(\"machine\" NEAR/3 \"learning\")").unwrap();
    }

    #[test]
    fn proximity_without_n_fails() {
        let err = validate("ta=(\"machine\" NEAR/ \"learning\")").unwrap_err();
        assert!(matches!(err.kind, CqlErrorKind::BadProximity(_)));
    }

    #[test]
    fn comparators_with_dates_pass() {
        validate("ta=solar AND pd>=2020").unwrap();
        validate("pd<=20251231").unwrap();
    }

    #[test]
    fn truncation_wildcards_are_allowed() {
        validate("ta=catalys*").unwrap();
        validate("ti=optimi?ation").unwrap();
    }

    #[test]
    fn fields_are_extracted_in_order() {
        let v = validate("ta=solar AND cpc=(H02S OR H02J) AND pd>=2020").unwrap();
        let names: Vec<&str> = v.fields.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(names, vec!["ta", "cpc", "pd"]);
    }

    #[test]
    fn quoted_field_codes_are_not_parsed_as_fields() {
        // The `ti=` inside the quote should NOT be parsed as a field code.
        let v = validate("ta=\"includes ti=phrase\" AND cpc=H02S").unwrap();
        let names: Vec<&str> = v.fields.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(names, vec!["ta", "cpc"]);
    }

    #[test]
    fn within_operator_extracts_field() {
        // EPO's recommended date-range form. The validator must extract `pd`
        // as a field with the comma-separated value, not silently skip it.
        let q = r#"pa=Apple AND pd within "20240101,20240107""#;
        let r = validate(q).expect("within form should validate");
        let pd_value = r
            .fields
            .iter()
            .find(|(k, _)| k == "pd")
            .map(|(_, v)| v.as_str());
        assert_eq!(pd_value, Some("20240101,20240107"));
    }

    #[test]
    fn within_is_case_insensitive() {
        for kw in ["within", "WITHIN", "Within", "WiThIn"] {
            let q = format!(r#"pd {kw} "20240101,20240107""#);
            let r = validate(&q).expect("case-insensitive within");
            assert_eq!(r.fields.len(), 1);
            assert_eq!(r.fields[0].0, "pd");
        }
    }

    #[test]
    fn within_prefix_in_other_word_does_not_trigger() {
        // `withinmask` shouldn't be parsed as `within` + `mask`.
        // (Contrived, but guards the boundary-char check in detect_comparator.)
        let q = "pd withinmask";
        // No comparator detected → pd treated as bare word, no fields.
        let r = validate(q).expect("malformed but parser-tolerant");
        assert!(r.fields.is_empty());
    }

    #[test]
    fn realistic_epo_query_validates() {
        let q = r#"ta=(("cross-modal" OR "multimodal") AND ("graph" OR "graph construction") AND ("similarity" OR "correlation")) AND cpc=(G06N OR G06F)"#;
        let v = validate(q).unwrap();
        assert_eq!(v.fields.len(), 2);
    }
}
