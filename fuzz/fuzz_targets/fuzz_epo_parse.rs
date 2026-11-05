#![no_main]
use epo::{
    parse_biblio, parse_citations, parse_claims, parse_cpc_titles, parse_description,
    parse_family, parse_image_inquiry, parse_search_results,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The XML parsers take any text.
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = parse_cpc_titles(text);
        let _ = parse_image_inquiry(text);
    }
    // The JSON parsers take any JSON value; none may panic.
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };
    let _ = parse_biblio(&json);
    let _ = parse_citations(&json);
    let _ = parse_family(&json);
    let _ = parse_search_results(&json);
    let _ = parse_description(&json, "EP1000000");
    let _ = parse_claims(&json, "EP1000000");
});
