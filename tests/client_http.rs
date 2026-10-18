//! Mock-HTTP unit tests for `EpoClient`. Cover OAuth2 caching/refresh, error
//! mapping (Auth / NotFound / RateLimit / Api), retry-then-success, and
//! bearer-token forwarding — all without hitting real EPO.
//!
//! Tokio runtime runs paused so the client's built-in 250ms / 500ms / …
//! backoff sleeps complete instantly. wiremock matchers verify request shape.

use epo::{ClientConfig, EpoClient, EpoError, FamilyMember, ThrottlingColor};
use serde_json::json;
use std::time::Duration;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "test-key";
const SECRET: &str = "test-secret";

fn ok_token(expires_in: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "access_token": "TOKEN-XYZ",
        "expires_in": expires_in,
        "token_type": "Bearer",
        "scope": "core",
        "status": "approved",
    }))
}

fn empty_biblio() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "ops:world-patent-data": {
            "exchange-documents": {
                "exchange-document": {
                    "@country": "EP",
                    "@kind": "B1",
                    "bibliographic-data": {
                        "invention-title": [{"@lang": "en", "$": "Test"}],
                        "publication-reference": {
                            "document-id": [{
                                "@document-id-type": "epodoc",
                                "doc-number": {"$": "EP1"}
                            }]
                        }
                    }
                }
            }
        }
    }))
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn token_is_cached_within_ttl() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/accesstoken"))
        .respond_with(ok_token("1200"))
        .expect(1) // exactly one token fetch despite two biblio calls
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/rest-services/published-data/publication/epodoc/EP1/biblio",
        ))
        .respond_with(empty_biblio())
        .expect(2)
        .mount(&server)
        .await;

    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    c.fetch_biblio("EP1").await.unwrap();
    c.fetch_biblio("EP1").await.unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn token_refreshes_after_expiry() {
    let server = MockServer::start().await;
    // expires_in: 0 → minus the 60s buffer → token is "expired" the moment
    // we fetch it, so the second biblio call must fetch a fresh token.
    Mock::given(method("POST"))
        .and(path("/auth/accesstoken"))
        .respond_with(ok_token("0"))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/rest-services/published-data/publication/epodoc/EP1/biblio",
        ))
        .respond_with(empty_biblio())
        .expect(2)
        .mount(&server)
        .await;

    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    c.fetch_biblio("EP1").await.unwrap();
    c.fetch_biblio("EP1").await.unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn token_endpoint_401_maps_to_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/accesstoken"))
        .respond_with(ResponseTemplate::new(401).set_body_string("invalid_client"))
        .mount(&server)
        .await;

    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    let err = c.fetch_biblio("EP1").await.unwrap_err();
    assert!(
        matches!(err, EpoError::Auth(_)),
        "expected Auth error, got {err:?}"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn biblio_404_maps_to_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_token("1200"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    assert!(matches!(
        c.fetch_biblio("ZZ99").await,
        Err(EpoError::NotFound)
    ));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn biblio_429_exhausts_retries_and_returns_rate_limit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_token("1200"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429))
        .expect(4) // backoff array has 4 attempts
        .mount(&server)
        .await;

    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    assert!(matches!(
        c.fetch_biblio("EP1").await,
        Err(EpoError::RateLimit)
    ));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn biblio_403_then_success_returns_ok() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_token("1200"))
        .mount(&server)
        .await;
    // First attempt: 403. Second attempt: 200. wiremock matches in
    // registration order, so the up-to-1 403 fires first.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(empty_biblio())
        .expect(1)
        .mount(&server)
        .await;

    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    let biblio = c.fetch_biblio("EP1").await.unwrap();
    assert_eq!(biblio.title, "Test");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn biblio_500_maps_to_api_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_token("1200"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream broken"))
        .mount(&server)
        .await;

    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    let err = c.fetch_biblio("EP1").await.unwrap_err();
    let EpoError::Api(msg) = err else {
        panic!("expected Api error, got {err:?}");
    };
    assert!(msg.contains("500"), "Api error should carry status: {msg}");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn search_404_returns_empty_results() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_token("1200"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/rest-services/published-data/search/biblio"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    let r = c.fetch_search("pa=Acme", 1, 10).await.unwrap();
    assert_eq!(r.total_count, 0);
    assert!(r.patents.is_empty());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn data_calls_send_bearer_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_token("1200"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(header("authorization", "Bearer TOKEN-XYZ"))
        .respond_with(empty_biblio())
        .expect(1)
        .mount(&server)
        .await;

    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    c.fetch_biblio("EP1").await.unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn fetch_description_parses_paragraphs() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_token("1200"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/rest-services/published-data/publication/epodoc/EP1/description",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ops:world-patent-data": {
                "ftxt:fulltext-documents": {
                    "ftxt:fulltext-document": {
                        "description": {
                            "@lang": "en",
                            "p": [
                                {"@num": "0001", "$": "The invention relates to widgets."},
                                {"@num": "0002", "$": "More specifically, sprocket widgets."}
                            ]
                        }
                    }
                }
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    let d = c.fetch_description("EP1").await.unwrap();
    assert_eq!(d.patent_id, "EP1");
    assert_eq!(d.language.as_deref(), Some("en"));
    assert_eq!(d.paragraphs.len(), 2);
    assert!(d.plain_text.contains("widgets"));
    assert!(d.plain_text.contains("\n\n"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn fetch_description_404_maps_to_not_found() {
    // Real EPO behavior: a patent with biblio data may still 404 on
    // description. Make sure callers can distinguish that cleanly.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_token("1200"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    assert!(matches!(
        c.fetch_description("EP1").await,
        Err(EpoError::NotFound)
    ));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn throttling_state_surfaces_from_response_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "access_token": "TOKEN-XYZ",
                    "expires_in": "1200",
                    "token_type": "Bearer",
                }))
                .insert_header(
                    "x-throttling-control",
                    "busy (images=green:100, inpadoc=green:45, other=green:1000, \
                     retrieval=green:100, search=green:15)",
                )
                .insert_header("x-individualquotaperhour-used", "20360")
                .insert_header("x-registeredquotaperweek-used", "1427448"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "ops:world-patent-data": {
                        "exchange-documents": {
                            "exchange-document": {
                                "@country": "EP", "@kind": "B1",
                                "bibliographic-data": {
                                    "invention-title": [{"@lang": "en", "$": "T"}],
                                    "publication-reference": {
                                        "document-id": [{
                                            "@document-id-type": "epodoc",
                                            "doc-number": {"$": "EP1"}
                                        }]
                                    }
                                }
                            }
                        }
                    }
                }))
                .insert_header(
                    "x-throttling-control",
                    "busy (inpadoc=yellow:5, search=red:2)",
                )
                .insert_header("x-registeredquotaperweek-used", "1500000"),
        )
        .mount(&server)
        .await;

    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    assert!(c.throttling_state().is_none(), "no calls yet");

    c.fetch_biblio("EP1").await.unwrap();

    // After the GET returned, the snapshot reflects the GET's headers
    // (since update_throttling overwrites on each successful call).
    let s = c.throttling_state().expect("populated after first call");
    assert_eq!(s.inpadoc_remaining(), Some(5));
    assert_eq!(s.search_remaining(), Some(2));
    assert_eq!(s.endpoints["inpadoc"].color, ThrottlingColor::Yellow);
    assert_eq!(s.endpoints["search"].color, ThrottlingColor::Red);
    assert_eq!(s.week_bytes_used, Some(1500000));
}

/// Regression: biblio + citations must come from a SINGLE HTTP round-trip.
/// EPO's biblio endpoint already includes the references-cited block, so
/// fetching them separately doubles request count + retrieval-quota cost.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn fetch_biblio_with_citations_one_round_trip() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_token("1200"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/rest-services/published-data/publication/epodoc/EP1/biblio",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ops:world-patent-data": {
                "exchange-documents": {
                    "exchange-document": {
                        "@country": "EP",
                        "@kind": "B1",
                        "bibliographic-data": {
                            "invention-title": [{"@lang": "en", "$": "Combined fetch"}],
                            "publication-reference": {
                                "document-id": [{
                                    "@document-id-type": "epodoc",
                                    "doc-number": {"$": "EP1"}
                                }]
                            },
                            "references-cited": {
                                "citation": {
                                    "@cited-phase": "search",
                                    "patcit": {
                                        "document-id": [{
                                            "@document-id-type": "epodoc",
                                            "doc-number": {"$": "US7700000"}
                                        }]
                                    }
                                }
                            }
                        }
                    }
                }
            }
        })))
        .expect(1) // KEY: exactly one biblio GET
        .mount(&server)
        .await;

    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    let (biblio, citations) = c.fetch_biblio_with_citations("EP1").await.unwrap();
    assert_eq!(biblio.title, "Combined fetch");
    assert_eq!(citations.cited.len(), 1);
    assert_eq!(citations.cited[0].patent_id, "US7700000");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn fetch_claims_parses_claim_set() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_token("1200"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/rest-services/published-data/publication/epodoc/EP1/claims",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ops:world-patent-data": {
                "ftxt:fulltext-documents": {
                    "ftxt:fulltext-document": {
                        "claims": {
                            "@lang": "en",
                            "claim": [
                                {
                                    "@id": "claim001",
                                    "@num": "0001",
                                    "claim-text": "1. A widget comprising a sprocket."
                                },
                                {
                                    "@id": "claim002",
                                    "@num": "0002",
                                    "claim-text": {"$": "2. The widget of claim 1, wherein the sprocket is gradient."}
                                }
                            ]
                        }
                    }
                }
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    let claims = c.fetch_claims("EP1").await.unwrap();
    assert_eq!(claims.patent_id, "EP1");
    assert_eq!(claims.language.as_deref(), Some("en"));
    assert_eq!(claims.claims.len(), 2);
    assert_eq!(claims.claims[0].id.as_deref(), Some("claim001"));
    assert!(claims.claims[0].text.contains("sprocket"));
    assert!(claims.plain_text.contains("\n\n"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn fetch_claims_404_maps_to_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_token("1200"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    assert!(matches!(
        c.fetch_claims("EP1").await,
        Err(EpoError::NotFound)
    ));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn fetch_citing_runs_search_with_ct_field() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_token("1200"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/rest-services/published-data/search/biblio"))
        .and(query_param("q", "ct=EP1000000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ops:world-patent-data": {
                "ops:biblio-search": {
                    "@total-result-count": "2",
                    "ops:range": {"@begin": "1", "@end": "2"},
                    "ops:search-result": {
                        "exchange-documents": [
                            {"exchange-document": {
                                "@country": "US", "@kind": "B2",
                                "bibliographic-data": {
                                    "publication-reference": {
                                        "document-id": [{
                                            "@document-id-type": "epodoc",
                                            "date": {"$": "20100401"},
                                            "doc-number": {"$": "US7700000"}
                                        }]
                                    }
                                }
                            }},
                            {"exchange-document": {
                                "@country": "EP", "@kind": "A1",
                                "bibliographic-data": {
                                    "publication-reference": {
                                        "document-id": [{
                                            "@document-id-type": "epodoc",
                                            "doc-number": {"$": "EP2000000"}
                                        }]
                                    }
                                }
                            }}
                        ]
                    }
                }
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    let citing = c.fetch_citing("EP1000000", 10).await.unwrap();
    assert_eq!(citing.len(), 2);
    assert_eq!(citing[0].patent_id, "US7700000");
    assert_eq!(citing[0].phase, "citing");
    assert_eq!(citing[0].date.as_deref(), Some("2010-04-01"));
    assert_eq!(citing[1].patent_id, "EP2000000");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn enrich_family_titles_fills_missing() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_token("1200"))
        .mount(&server)
        .await;
    // Each family-member biblio fetch returns the standard empty_biblio()
    // shape — title "Test" — so any member with empty title gets filled.
    Mock::given(method("GET"))
        .respond_with(empty_biblio())
        .mount(&server)
        .await;

    let mut family = vec![
        FamilyMember {
            patent_id: "EP1".into(),
            country: "EP".into(),
            kind: "A1".into(),
            title: String::new(),
            publication_date: None,
        },
        FamilyMember {
            patent_id: "US222".into(),
            country: "US".into(),
            kind: "B2".into(),
            title: "Already filled".into(), // skipped, no fetch
            publication_date: None,
        },
        FamilyMember {
            patent_id: "DE3".into(),
            country: "DE".into(),
            kind: "A1".into(),
            title: String::new(),
            publication_date: None,
        },
    ];
    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    let filled = c.enrich_family_titles(&mut family).await;
    assert_eq!(filled, 2, "two members had empty titles");
    assert_eq!(family[0].title, "Test");
    assert_eq!(family[1].title, "Already filled");
    assert_eq!(family[2].title, "Test");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn client_config_overrides_retry_and_pacing() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_token("1200"))
        .mount(&server)
        .await;
    // Custom config: only TWO retry attempts instead of four.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429))
        .expect(2)
        .mount(&server)
        .await;

    let cfg = ClientConfig {
        base_url: server.uri(),
        retry_backoff: vec![Duration::from_millis(0), Duration::from_millis(0)],
        // Zero pacers so the test is fast under paused-clock.
        inpadoc_interval: Duration::from_millis(0),
        search_interval: Duration::from_millis(0),
        retrieval_interval: Duration::from_millis(0),
        other_interval: Duration::from_millis(0),
        token_refresh_buffer: Duration::from_secs(60),
        weekly_warn_bytes: u64::MAX,
        max_concurrent: 8,
    };
    let c = EpoClient::with_config(KEY.into(), SECRET.into(), cfg);
    assert!(matches!(
        c.fetch_biblio("EP1").await,
        Err(EpoError::RateLimit)
    ));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn concurrent_calls_share_one_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ok_token("1200"))
        .expect(1) // mutex serialises; second caller sees cached token
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(empty_biblio())
        .expect(2)
        .mount(&server)
        .await;

    let c = std::sync::Arc::new(EpoClient::new(
        KEY.into(),
        SECRET.into(),
        Some(server.uri()),
    ));
    let c2 = c.clone();
    let (a, b) = tokio::join!(c.fetch_biblio("EP1"), c2.fetch_biblio("EP2"));
    a.unwrap();
    b.unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn fetch_cpc_titles_gets_each_symbol_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/accesstoken"))
        .respond_with(ok_token("1200"))
        .mount(&server)
        .await;
    // EPO's answer for H01M10/6556 from the live run.
    Mock::given(method("GET"))
        .and(path("/rest-services/classification/cpc/H01M10/6556"))
        .and(query_param("ancestors", ""))
        .and(header("accept", "application/cpc+xml"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<?xml version="1.0" encoding="utf-8" standalone="yes"?><?xml-stylesheet type='text/xsl' href='../../../../style/cpc.xsl' ?><ops:world-patent-data xmlns:ops="http://ops.epo.org" xmlns:reg="http://www.epo.org/register" xmlns:cpcdef="http://www.epo.org/cpcdefinition" xmlns:cpc="http://www.epo.org/cpcexport" xmlns:xlink="http://www.w3.org/1999/xlink"><ops:classification-scheme><ops:cpc><cpc:class-scheme scheme-type="cpc" export-date="2026-08-01"><cpc:classification-item level="11" additional-only="false" sort-key="H01M10/6556" not-allocatable="false" breakdown-code="false" status="published" link-file="classification/cpc/H01M10/6556" ipc-concordant="H01M10/6556" definition-exists="false" c-set-subsequent-allowed="true" has-children="true" date-revised="2015-04-01"><cpc:classification-symbol>H01M10/6556</cpc:classification-symbol><cpc:class-title><cpc:title-part><cpc:text>Solid parts with flow channel passages or pipes for heat exchange </cpc:text><cpc:explanation><cpc:text>closed pipes <cpc:class-ref scheme="cpc">H01M10/6552</cpc:class-ref></cpc:text></cpc:explanation></cpc:title-part></cpc:class-title><cpc:meta-data>+</cpc:meta-data></cpc:classification-item></cpc:class-scheme></ops:cpc></ops:classification-scheme></ops:world-patent-data>"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/rest-services/classification/cpc/H01M10/659"))
        .and(query_param("ancestors", ""))
        .and(header("accept", "application/cpc+xml"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<ops:world-patent-data xmlns:ops="http://ops.epo.org" xmlns:cpc="http://www.epo.org/cpcexport">
              <ops:classification-scheme><ops:cpc><cpc:class-scheme scheme-type="cpc">
                <cpc:classification-item level="8">
                  <cpc:classification-symbol>H01M10/65</cpc:classification-symbol>
                  <cpc:class-title><cpc:title-part><cpc:text>Means for temperature control structurally associated with the cells</cpc:text></cpc:title-part></cpc:class-title>
                  <cpc:classification-item level="9">
                    <cpc:classification-symbol>H01M10/659</cpc:classification-symbol>
                    <cpc:class-title><cpc:title-part><cpc:text>by heat storage or buffering, e.g. heat capacity or liquid-solid phase changes or transition</cpc:text></cpc:title-part></cpc:class-title>
                  </cpc:classification-item>
                </cpc:classification-item>
              </cpc:class-scheme></ops:cpc></ops:classification-scheme>
            </ops:world-patent-data>"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    let titles = c
        .fetch_cpc_titles(&["H01M10/6556", "h01m 10/659", "H01M10/6556"])
        .await
        .unwrap();
    assert_eq!(titles.len(), 2, "only the asked symbols: {titles:?}");
    assert_eq!(
        titles["H01M10/6556"],
        "Solid parts with flow channel passages or pipes for heat exchange"
    );
    assert_eq!(
        titles["H01M10/659"],
        "Means for temperature control structurally associated with the cells by heat storage or buffering, e.g. heat capacity or liquid-solid phase changes or transition"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn fetch_cpc_titles_skips_a_failed_symbol_and_keeps_the_rest() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/accesstoken"))
        .respond_with(ok_token("1200"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/rest-services/classification/cpc/A01B"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<ops:world-patent-data xmlns:ops="http://ops.epo.org" xmlns:cpc="http://www.epo.org/cpcexport">
              <ops:classification-scheme><ops:cpc><cpc:class-scheme scheme-type="cpc">
                <cpc:classification-item level="5">
                  <cpc:classification-symbol>A01B</cpc:classification-symbol>
                  <cpc:class-title><cpc:title-part><cpc:text>SOIL WORKING IN AGRICULTURE OR FORESTRY</cpc:text></cpc:title-part></cpc:class-title>
                </cpc:classification-item>
              </cpc:class-scheme></ops:cpc></ops:classification-scheme>
            </ops:world-patent-data>"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/rest-services/classification/cpc/A01C"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/rest-services/classification/cpc/A01D"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"error": {}}"#))
        .expect(1)
        .mount(&server)
        .await;

    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    let titles = c
        .fetch_cpc_titles(&["A01B", "A01C", "A01D", "A01B/../../published-data"])
        .await
        .unwrap();
    assert_eq!(titles.len(), 1, "{titles:?}");
    assert_eq!(titles["A01B"], "SOIL WORKING IN AGRICULTURE OR FORESTRY");
    let gets = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "GET")
        .count();
    assert_eq!(gets, 3, "the symbol with `..` sends no request");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn fetch_cpc_titles_stops_on_an_exhausted_rate_limit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/accesstoken"))
        .respond_with(ok_token("1200"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;

    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    assert!(matches!(
        c.fetch_cpc_titles(&["A01B", "A01C"]).await,
        Err(EpoError::RateLimit)
    ));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn fetch_cpc_titles_404_and_empty_list_return_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/accesstoken"))
        .respond_with(ok_token("1200"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/rest-services/classification/cpc/Z99Z99/99"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    let c = EpoClient::new(KEY.into(), SECRET.into(), Some(server.uri()));
    assert!(c.fetch_cpc_titles(&["Z99Z99/99"]).await.unwrap().is_empty());
    assert!(
        c.fetch_cpc_titles(&[]).await.unwrap().is_empty(),
        "no request"
    );
}
