#![no_main]
use libfuzzer_sys::fuzz_target;
use epo::{
    parse_biblio, extract_text_by_lang, extract_applicant, extract_date,
    extract_classifications, parse_citations, parse_family, parse_search_results,
};

fuzz_target!(|data: &[u8]| {
    // Try to parse as JSON — if it's not valid JSON, skip
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };
    // Feed to all 8 EPO parsers — none should panic
    let _ = parse_biblio(&json);
    let _ = extract_text_by_lang(&json);
    let _ = extract_applicant(&json);
    let _ = extract_date(&json);
    let _ = extract_classifications(&json);
    let _ = parse_citations(&json);
    let _ = parse_family(&json);
    let _ = parse_search_results(&json);
});
