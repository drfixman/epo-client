# Changelog

All notable changes to this crate are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning follows
[Semantic Versioning](https://semver.org/).

## [0.0.3] - 2026-10-18

- `fetch_cpc_titles`: CPC scheme titles for a list of codes, one
  `GET /classification/cpc/{symbol}?ancestors` per code (XML answer), with
  `parse_cpc_titles` and `normalize_cpc_symbol`. A title that continues
  its parent's title ("by heat storage or buffering, …") gets the
  parent's title in front.

## [0.0.2] - 2026-05-18

- `ClientConfig::max_concurrent` caps in-flight requests across every
  endpoint (default 8, `0` counts as 1). The per-endpoint pacers still
  apply to each request.

## [0.0.1] - 2026-05-03

Initial public release.

- Async client for EPO OPS v3.2: `fetch_biblio`, `fetch_family`,
  `fetch_citations`, `fetch_citing`, `fetch_search`, `fetch_description`,
  `fetch_claims`, plus `fetch_biblio_with_citations` and
  `enrich_family_titles` helpers.
- OAuth2 client-credentials with token caching + automatic refresh.
- Tolerant JSON parsing for EPO's nested + single-or-array shapes,
  multilingual fields, and structured CPC blocks.
- `ClientConfig` with per-endpoint pacing matching EPO's documented
  per-minute ceilings, plus exponential backoff on 403/429.
- `ThrottlingState` snapshot exposed via `EpoClient::throttling_state()`,
  parsed from EPO's `x-throttling-control` + quota headers.
- `validate_cql` syntax validator: balanced parens/quotes, known field
  codes, comparators, `within` ranges, `NEAR/n` proximity.
- Typed `EpoError`: `Auth` / `NotFound` / `RateLimit` / `Api(status, body)` /
  `Network`.

[0.0.3]: https://github.com/drfixman/epo-client/releases/tag/v0.0.3
[0.0.2]: https://github.com/drfixman/epo-client/releases/tag/v0.0.2
[0.0.1]: https://github.com/drfixman/epo-client/releases/tag/v0.0.1
