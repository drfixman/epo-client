//! Live EPO OPS integration tests — hit https://ops.epo.org/3.2 directly.
//!
//! All tests are #[ignore]'d. Run with:
//!   cargo test -p epo --test real_epo -- --ignored --test-threads=1 --nocapture
//! Requires EPO_CONSUMER_KEY and EPO_CONSUMER_SECRET in the environment.
//!
//! Each test prints both the raw EPO JSON response and the parsed struct so
//! you can verify the parser against the wire shape; assertions are kept
//! minimal so one weird patent doesn't hide later ones.

use epo::{
    EpoClient, EpoError, parse_biblio, parse_citations, parse_cpc_titles, parse_family,
    parse_search_results, validate_cql,
};
use tracing_subscriber::EnvFilter;

const BASE: &str = "https://ops.epo.org/3.2";
const TEST_PATENT: &str = "EP1000000";

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("epo=debug,real_epo=info,reqwest=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_test_writer()
        .try_init();
}

fn env_creds() -> (String, String) {
    let key = std::env::var("EPO_CONSUMER_KEY")
        .expect("EPO_CONSUMER_KEY not set; export your real EPO OPS consumer key");
    let secret = std::env::var("EPO_CONSUMER_SECRET")
        .expect("EPO_CONSUMER_SECRET not set; export your real EPO OPS consumer secret");
    (key, secret)
}

async fn auth(http: &reqwest::Client, key: &str, secret: &str) -> String {
    let resp = http
        .post(format!("{BASE}/auth/accesstoken"))
        .basic_auth(key, Some(secret))
        .form(&[("grant_type", "client_credentials")])
        .send()
        .await
        .expect("token request failed");
    let status = resp.status();
    let json: serde_json::Value = resp.json().await.expect("token body should be JSON");
    eprintln!(
        "\n=== auth (raw token response) ===\nstatus: {status}\nbody:\n{}\n",
        redact_token(&json)
    );
    json["access_token"]
        .as_str()
        .expect("access_token field present")
        .to_string()
}

fn redact_token(json: &serde_json::Value) -> String {
    const SENSITIVE: &[&str] = &[
        "access_token",
        "client_id",
        "developer.email",
        "application_name",
        "organization_name",
    ];
    let mut copy = json.clone();
    if let Some(obj) = copy.as_object_mut() {
        for key in SENSITIVE {
            if obj.contains_key(*key) {
                obj.insert((*key).into(), "<REDACTED>".into());
            }
        }
    }
    serde_json::to_string_pretty(&copy).unwrap_or_else(|_| copy.to_string())
}

async fn get_json(
    http: &reqwest::Client,
    token: &str,
    path: &str,
    query: &[(&str, &str)],
    label: &str,
) -> (reqwest::StatusCode, serde_json::Value) {
    let url = format!("{BASE}{path}");
    let mut req = http
        .get(&url)
        .header("Accept", "application/json")
        .bearer_auth(token);
    if !query.is_empty() {
        req = req.query(query);
    }
    let resp = req.send().await.expect("HTTP request failed");
    let status = resp.status();
    let text = resp.text().await.expect("body should read");
    let json = serde_json::from_str::<serde_json::Value>(&text)
        .unwrap_or_else(|_| serde_json::Value::String(text.clone()));
    eprintln!(
        "\n=== {label} (raw EPO response) ===\nurl: {url}\nquery: {query:?}\nstatus: {status}\nbody:\n{}\n",
        serde_json::to_string_pretty(&json).unwrap_or_else(|_| text.clone())
    );
    (status, json)
}

fn dump_parsed<T: std::fmt::Debug>(label: &str, parsed: &T) {
    eprintln!("\n=== {label} (parsed) ===\n{parsed:#?}\n");
}

#[tokio::test]
#[ignore]
async fn biblio_real() {
    init_tracing();
    let (key, secret) = env_creds();
    let http = reqwest::Client::new();
    let token = auth(&http, &key, &secret).await;

    let path = format!("/rest-services/published-data/publication/epodoc/{TEST_PATENT}/biblio");
    let (status, json) = get_json(
        &http,
        &token,
        &path,
        &[],
        &format!("biblio_real ({TEST_PATENT})"),
    )
    .await;
    assert!(status.is_success(), "biblio status not 2xx: {status}");

    let biblio = parse_biblio(&json);
    dump_parsed(&format!("biblio_real ({TEST_PATENT})"), &biblio);

    assert!(!biblio.title.is_empty(), "title should be non-empty");
    assert!(
        !biblio.abstract_text.is_empty(),
        "abstract_text should be non-empty (parser bug fix)"
    );
    assert!(biblio.kind_code.is_some(), "kind_code should be present");
    assert!(
        biblio.publication_date.is_some(),
        "publication_date should be present"
    );
    assert!(
        !biblio.classification.is_empty() || !biblio.cpc_classifications.is_empty(),
        "should have at least one IPC or CPC code"
    );
}

/// Robustness check across diverse real patents — different decades, kinds,
/// countries. Prints both raw + parsed for each. Collects field-level surprises
/// (empty title, missing kind, no classification, …); panics at the end with
/// the full list so one bad patent doesn't hide later ones.
#[tokio::test]
#[ignore]
async fn biblio_variants_real() {
    init_tracing();
    let (key, secret) = env_creds();
    let http = reqwest::Client::new();
    let token = auth(&http, &key, &secret).await;

    let patents = [
        "EP1000000",  // 2000-era Dutch grant (A1+B1)
        "EP2000000",  // 2008-era 2-millionth
        "EP3000000",  // 2016-era 3-millionth
        "EP4000000",  // very recent
        "US10000000", // US patent via EPO OPS
    ];

    let mut failures: Vec<String> = Vec::new();

    for patent_id in patents {
        let path = format!("/rest-services/published-data/publication/epodoc/{patent_id}/biblio");
        let (status, json) = get_json(
            &http,
            &token,
            &path,
            &[],
            &format!("biblio_variants_real ({patent_id})"),
        )
        .await;

        if !status.is_success() {
            failures.push(format!("{patent_id}: HTTP {status}"));
            continue;
        }

        let biblio = parse_biblio(&json);
        dump_parsed(&format!("biblio_variants_real ({patent_id})"), &biblio);

        if biblio.title.is_empty() {
            failures.push(format!("{patent_id}: empty title"));
        }
        if biblio.kind_code.is_none() {
            failures.push(format!("{patent_id}: no kind_code"));
        }
        if biblio.publication_date.is_none() {
            failures.push(format!("{patent_id}: no publication_date"));
        }
        if biblio.classification.is_empty() && biblio.cpc_classifications.is_empty() {
            failures.push(format!(
                "{patent_id}: zero classifications (IPC + CPC both empty)"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "Variants check found issues:\n  - {}",
        failures.join("\n  - ")
    );
}

#[tokio::test]
#[ignore]
async fn family_real() {
    init_tracing();
    let (key, secret) = env_creds();
    let http = reqwest::Client::new();
    let token = auth(&http, &key, &secret).await;

    let path = format!("/rest-services/family/publication/epodoc/{TEST_PATENT}");
    let (status, json) = get_json(
        &http,
        &token,
        &path,
        &[],
        &format!("family_real ({TEST_PATENT})"),
    )
    .await;
    assert!(status.is_success(), "family status not 2xx: {status}");

    let family = parse_family(&json);
    dump_parsed(&format!("family_real ({TEST_PATENT})"), &family);

    assert!(
        !family.is_empty(),
        "family should have at least one member for {TEST_PATENT}"
    );
    for m in &family {
        assert!(!m.country.is_empty(), "family member country empty: {m:?}");
        assert!(
            !m.patent_id.is_empty(),
            "family member patent_id empty: {m:?}"
        );
    }
}

#[tokio::test]
#[ignore]
async fn citations_real() {
    init_tracing();
    let (key, secret) = env_creds();
    let http = reqwest::Client::new();
    let token = auth(&http, &key, &secret).await;

    let path = format!("/rest-services/published-data/publication/epodoc/{TEST_PATENT}/biblio");
    let (status, json) = get_json(
        &http,
        &token,
        &path,
        &[],
        &format!("citations_real ({TEST_PATENT}) [reads biblio endpoint]"),
    )
    .await;
    assert!(status.is_success(), "biblio status not 2xx: {status}");

    let citations = parse_citations(&json);
    dump_parsed(&format!("citations_real ({TEST_PATENT})"), &citations);

    // `citing` (forward) is empty by design — biblio endpoint doesn't expose it.
    for c in &citations.cited {
        assert!(!c.patent_id.is_empty(), "cited.patent_id empty: {c:?}");
    }
}

#[tokio::test]
#[ignore]
async fn search_real() {
    init_tracing();
    let (key, secret) = env_creds();
    let http = reqwest::Client::new();
    let token = auth(&http, &key, &secret).await;

    // EPO returns CLIENT.FuzzyDateRanges (HTTP 413) on `pd>=X AND pd<=Y`.
    // Their docs require `within "start,end"` for date ranges.
    let cql = r#"pa=Apple AND pd within "20240101,20240107""#;
    let validated = validate_cql(cql).expect("our CQL validator should accept this query");
    eprintln!("\n=== search_real validate_cql ===\n{validated:#?}\n");

    let path = "/rest-services/published-data/search/biblio";
    let (status, json) = get_json(
        &http,
        &token,
        path,
        &[("q", cql), ("Range", "1-10")],
        "search_real",
    )
    .await;
    assert!(status.is_success(), "search status not 2xx: {status}");

    let results = parse_search_results(&json);
    dump_parsed("search_real", &results);

    assert!(
        results.total_count > 0,
        "Apple, 2024-01-01..07 should yield at least one patent"
    );
    assert!(
        !results.patents.is_empty(),
        "patents Vec should be non-empty"
    );
    assert_eq!(results.range.0, 1, "range start should match request");
    assert!(results.range.1 <= 10, "range end should be <= request end");
}

/// Description endpoint smoke test. Description availability is patchy —
/// many older or non-major-jurisdiction patents are biblio-only. We accept
/// both `Ok(non-empty)` and `Err(NotFound)` as valid live outcomes; the
/// raw response is printed either way for diagnosis.
#[tokio::test]
#[ignore]
async fn description_real() {
    init_tracing();
    let (key, secret) = env_creds();
    let http = reqwest::Client::new();
    let token = auth(&http, &key, &secret).await;

    let path =
        format!("/rest-services/published-data/publication/epodoc/{TEST_PATENT}/description");
    let (status, json) = get_json(
        &http,
        &token,
        &path,
        &[],
        &format!("description_real ({TEST_PATENT})"),
    )
    .await;

    if status.as_u16() == 404 {
        eprintln!(
            "=== description_real ({TEST_PATENT}) ===\n\
             EPO returned 404 — no description available for this patent.\n\
             This is normal for many older / non-major-jurisdiction filings.\n"
        );
        return;
    }
    assert!(status.is_success(), "description status not 2xx: {status}");

    let parsed = epo::parse_description(&json, TEST_PATENT);
    eprintln!(
        "=== description_real ({TEST_PATENT}) parsed ===\n\
         language: {:?}\n\
         paragraphs: {} entries\n\
         plain_text length: {} chars\n\
         first 500 chars:\n{}\n",
        parsed.language,
        parsed.paragraphs.len(),
        parsed.plain_text.len(),
        parsed.plain_text.chars().take(500).collect::<String>()
    );

    assert!(
        !parsed.paragraphs.is_empty(),
        "200 OK but no paragraphs — parser didn't find description.p"
    );
    assert!(
        !parsed.plain_text.is_empty(),
        "all paragraphs were empty after trim — suspicious"
    );
}

/// Claims endpoint smoke test. Like description, claims are not always
/// available — older or smaller-jurisdiction filings may 404. Both
/// outcomes are valid; the raw response is printed either way.
#[tokio::test]
#[ignore]
async fn claims_real() {
    init_tracing();
    let (key, secret) = env_creds();
    let http = reqwest::Client::new();
    let token = auth(&http, &key, &secret).await;

    let path = format!("/rest-services/published-data/publication/epodoc/{TEST_PATENT}/claims");
    let (status, json) = get_json(
        &http,
        &token,
        &path,
        &[],
        &format!("claims_real ({TEST_PATENT})"),
    )
    .await;

    if status.as_u16() == 404 {
        eprintln!(
            "=== claims_real ({TEST_PATENT}) ===\n\
             EPO returned 404 — no claims available for this patent.\n"
        );
        return;
    }
    assert!(status.is_success(), "claims status not 2xx: {status}");

    let parsed = epo::parse_claims(&json, TEST_PATENT);
    eprintln!(
        "=== claims_real ({TEST_PATENT}) parsed ===\n\
         language: {:?}\n\
         claims: {} entries\n\
         plain_text length: {} chars\n\
         first claim:\n{}\n",
        parsed.language,
        parsed.claims.len(),
        parsed.plain_text.len(),
        parsed
            .claims
            .first()
            .map(|c| c.text.as_str())
            .unwrap_or("(none)")
    );

    assert!(
        !parsed.claims.is_empty(),
        "200 OK but no claims — parser didn't find claims.claim"
    );
}

/// Exercises `EpoClient::fetch_biblio_with_citations` end-to-end.
/// Verifies the single-round-trip contract against real EPO: one HTTP call
/// produces both biblio AND citations populated from the same response.
#[tokio::test]
#[ignore]
async fn biblio_with_citations_real() {
    init_tracing();
    let (key, secret) = env_creds();
    let client = EpoClient::new(key, secret, None);
    let result = client.fetch_biblio_with_citations(TEST_PATENT).await;
    eprintln!("\n=== biblio_with_citations_real ({TEST_PATENT}) ===\n{result:#?}\n");

    let (biblio, citations) = result.expect("combined fetch should succeed");
    assert!(!biblio.title.is_empty(), "biblio should have a title");
    assert!(
        !biblio.classification.is_empty() || !biblio.cpc_classifications.is_empty(),
        "biblio should have at least one classification"
    );
    // EP1000000's A1 carries 3 examiner citations; the combined fetch must
    // see them (parse_citations walks the same JSON the biblio came from).
    assert!(
        !citations.cited.is_empty(),
        "EP1000000 has known cited references"
    );
}

/// Exercises `EpoClient::fetch_citing` against real EPO. Forward citations
/// for old patents can be sparse — this only asserts the call doesn't fail
/// and prints the citing patents for diagnosis.
#[tokio::test]
#[ignore]
async fn citing_real() {
    init_tracing();
    let (key, secret) = env_creds();
    let client = EpoClient::new(key, secret, None);
    let result = client.fetch_citing(TEST_PATENT, 25).await;
    eprintln!("\n=== citing_real ({TEST_PATENT}) ===\n{result:#?}\n");

    let citing = result.expect("fetch_citing should succeed (may return empty)");
    eprintln!(
        "Found {} forward citations for {TEST_PATENT}; first 5:",
        citing.len()
    );
    for c in citing.iter().take(5) {
        eprintln!(
            "  - {} ({}) by {}",
            c.patent_id,
            c.date.as_deref().unwrap_or("?"),
            c.name.as_deref().unwrap_or("?")
        );
    }
}

/// Exercises `EpoClient::enrich_family_titles` against real EPO. Calls
/// `fetch_family` first (titles empty per EPO design), then enriches
/// per-member via `fetch_biblio` fan-out.
#[tokio::test]
#[ignore]
async fn enrich_family_titles_real() {
    init_tracing();
    let (key, secret) = env_creds();
    let client = EpoClient::new(key, secret, None);

    let mut family = client
        .fetch_family(TEST_PATENT)
        .await
        .expect("fetch_family should succeed");

    eprintln!(
        "\n=== enrich_family_titles_real ({TEST_PATENT}) BEFORE ({} members) ===",
        family.len()
    );
    for m in &family {
        eprintln!("  {:<14} [{:<3}] title={:?}", m.patent_id, m.kind, m.title);
    }

    let filled = client.enrich_family_titles(&mut family).await;
    eprintln!("\nFilled {filled} titles via per-member biblio fetches.\n");

    eprintln!("=== enrich_family_titles_real ({TEST_PATENT}) AFTER ===");
    for m in &family {
        eprintln!("  {:<14} [{:<3}] title={:?}", m.patent_id, m.kind, m.title);
    }

    assert!(
        !family.is_empty(),
        "family should be non-empty for {TEST_PATENT}"
    );
    assert!(
        filled > 0,
        "at least one family member should have had a fillable title"
    );
}

/// Verifies `EpoClient` maps a real 404 to `EpoError::NotFound`. This one
/// uses the high-level client (not the raw helper) because the assertion is
/// about the client's status-code → error-variant mapping, not the parser.
#[tokio::test]
#[ignore]
async fn not_found_real() {
    init_tracing();
    let (key, secret) = env_creds();
    let client = EpoClient::new(key, secret, None);
    let result = client.fetch_biblio("ZZ99999999").await;
    eprintln!("\n=== not_found_real (parsed) ===\n{result:#?}\n");

    match result {
        Err(EpoError::NotFound) => {}
        Err(other) => panic!("expected EpoError::NotFound, got: {other:?}"),
        Ok(b) => panic!("expected NotFound for bogus ID, got biblio: {b:?}"),
    }
}

/// The CPC retrieval service, in the form the OPS 3.2 reference guide
/// documents (§3.6.1): one symbol per request,
/// `GET /classification/cpc/{symbol}?ancestors` with
/// `Accept: application/cpc+xml`. H01M10/659's own title starts with
/// "by heat storage …", so its full title needs the ancestors.
#[tokio::test]
#[ignore]
async fn cpc_titles_real() {
    init_tracing();
    let (key, secret) = env_creds();
    let http = reqwest::Client::new();
    let token = auth(&http, &key, &secret).await;
    let url = format!("{BASE}/rest-services/classification/cpc/H01M10/659?ancestors");
    let resp = http
        .get(&url)
        .header("Accept", "application/cpc+xml")
        .bearer_auth(&token)
        .send()
        .await
        .expect("HTTP request failed");
    let status = resp.status();
    let xml = resp.text().await.expect("body should read");
    eprintln!(
        "\n=== cpc_titles_real (raw EPO response) ===\nurl: {url}\nstatus: {status}\nbody:\n{xml}\n"
    );
    dump_parsed(
        "cpc_titles_real (parse_cpc_titles)",
        &parse_cpc_titles(&xml),
    );

    let client = EpoClient::new(key, secret, None);
    let titles = client
        .fetch_cpc_titles(&["H01M10/6556", "H01M10/659"])
        .await
        .expect("fetch_cpc_titles");
    dump_parsed("cpc_titles_real (fetch_cpc_titles)", &titles);
    assert!(titles.contains_key("H01M10/6556"), "{titles:?}");
    assert!(
        titles["H01M10/659"].starts_with(char::is_uppercase),
        "the parent's title comes first: {titles:?}"
    );
}
