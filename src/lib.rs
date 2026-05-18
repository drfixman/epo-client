//! Async client for the [EPO Open Patent Services](https://ops.epo.org) v3.2 REST API.
//!
//! Handles OAuth2 token caching, bibliographic / family / citation / search
//! fetching, deeply nested JSON response parsing, and exponential backoff on
//! 403/429. Also exposes a lightweight CQL syntax validator so callers can
//! pre-flight queries before sending them.
//!
//! # Quick start
//!
//! ```no_run
//! use epo::{EpoClient, EpoError};
//!
//! # async fn run() -> Result<(), EpoError> {
//! let client = EpoClient::new(
//!     std::env::var("EPO_CONSUMER_KEY").unwrap(),
//!     std::env::var("EPO_CONSUMER_SECRET").unwrap(),
//!     None, // defaults to https://ops.epo.org/3.2
//! );
//!
//! let biblio = client.fetch_biblio("EP1000000").await?;
//! println!("{}: {}", biblio.title, biblio.assignee.as_deref().unwrap_or(""));
//! # Ok(())
//! # }
//! ```
//!
//! # CQL pre-flight
//!
//! ```
//! use epo::validate_cql;
//!
//! // Date ranges must use `within`, not `>=`/`<=` — EPO rejects the latter.
//! let q = r#"pa=Apple AND pd within "20240101,20240107""#;
//! let v = validate_cql(q).expect("syntactically valid");
//! assert_eq!(v.fields[0], ("pa".into(), "Apple".into()));
//! ```
//!
//! # Endpoints
//!
//! - [`EpoClient::fetch_biblio`] — bibliographic data (title, abstract, applicants, classifications, dates).
//! - [`EpoClient::fetch_description`] — full-text description body, paragraph-level. Separate quota; may 404 even when biblio works.
//! - [`EpoClient::fetch_claims`] — full claim set, claim-level. Same separate quota as description.
//! - [`EpoClient::fetch_family`] — INPADOC family members.
//! - [`EpoClient::fetch_citations`] — backward citations from the biblio endpoint.
//! - [`EpoClient::fetch_citing`] — forward citations via CQL search.
//! - [`EpoClient::fetch_search`] — CQL search with pagination.
//!
//! All four return typed [`EpoError`] variants on failure, with retry built in
//! for `403`/`429`. The token endpoint response is cached and refreshed
//! automatically when it nears expiry.
//!
//! # Credentials
//!
//! You need an EPO OPS consumer key + secret from
//! <https://developers.epo.org>. The free tier limits weekly traffic by bytes;
//! the client honours the documented 4 req/s pacing internally.

use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};
use tracing::{Span, debug, field, info, instrument, warn};

mod validator;
pub use validator::{CqlError, CqlErrorKind, FIELD_CODES, ValidatedCql, validate as validate_cql};

const DEFAULT_EPO_BASE: &str = "https://ops.epo.org/3.2";

/// Errors returned by every [`EpoClient`] method.
///
/// Variants distinguish recoverable failures (`RateLimit` — try again) from
/// terminal ones (`NotFound`, `Auth`) and from transport noise (`Network`)
/// vs. server-side anomalies (`Api`).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum EpoError {
    /// OAuth2 token endpoint rejected the credentials, or the response was
    /// missing a usable `access_token`. The contained string carries the
    /// status + body for debugging.
    Auth(String),
    /// HTTP 404 from a data endpoint — the patent isn't in EPO's index.
    NotFound,
    /// HTTP 403 or 429 — rate limit hit. The client retries automatically
    /// (per [`ClientConfig::retry_backoff`]); this variant means the retry
    /// schedule was exhausted.
    RateLimit,
    /// Any other non-success HTTP status (4xx other than 403/404/429, or
    /// 5xx). Contains the status code and response body.
    Api(String),
    /// Transport failure (DNS, connection refused, TLS, timeout). Contains
    /// the underlying [`reqwest`] error message.
    Network(String),
}

impl std::fmt::Display for EpoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EpoError::Auth(msg) => write!(f, "EPO auth error: {msg}"),
            EpoError::NotFound => write!(f, "Patent not found"),
            EpoError::RateLimit => write!(f, "EPO rate limit exceeded"),
            EpoError::Api(msg) => write!(f, "EPO API error: {msg}"),
            EpoError::Network(msg) => write!(f, "Network error: {msg}"),
        }
    }
}

impl std::error::Error for EpoError {}

/// One member of an INPADOC patent family. Returned by [`EpoClient::fetch_family`].
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FamilyMember {
    /// Publication number in epodoc format (e.g. `EP1000000`, `US12131170`).
    pub patent_id: String,
    /// ISO country code of the publication office (`EP`, `US`, `JP`, …).
    pub country: String,
    /// Publication kind code (`A1`, `B1`, `T1`, `C2`, …). Country-specific
    /// scheme — `B` is granted in EP/US, `C` in NL, `T` in AT/DE for translations.
    pub kind: String,
    /// EPO's family endpoint does not return invention titles. Always empty
    /// from [`parse_family`]; use [`EpoClient::enrich_family_titles`] to
    /// fan out per-member biblio fetches if you need them.
    pub title: String,
    /// Publication date (`YYYY-MM-DD`) of this publication, when EPO
    /// supplied one.
    pub publication_date: Option<String>,
}

/// One backward or forward citation. Returned in [`Citations::cited`] from
/// [`EpoClient::fetch_citations`] (with `phase = "search" | "examination" | …`)
/// and in the result of [`EpoClient::fetch_citing`] (with `phase = "citing"`).
#[derive(Debug, Clone, serde::Serialize)]
#[non_exhaustive]
pub struct Citation {
    /// Cited patent's publication number in epodoc format.
    pub patent_id: String,
    /// Citation phase from `@cited-phase`: `national-search-report`,
    /// `examination`, `opposition`, `undefined`, … or the literal `"citing"`
    /// for forward citations from [`EpoClient::fetch_citing`].
    pub phase: String,
    /// EPO citation category from `category.$`: `X` (novelty / inventive step),
    /// `Y` (combination), `A` (technology background), `E` (earlier patent),
    /// `D` (cited in application), `P`/`L`/`O`/`T` (rare). X and Y are the
    /// strongest prior-art signals. `None` for forward citations.
    pub category: Option<String>,
    /// `@cited-by`: `examiner`, `applicant`, `third-party`, `unknown`. `None`
    /// for forward citations.
    pub cited_by: Option<String>,
    /// Publication date of the cited patent (`YYYY-MM-DD`), when supplied.
    pub date: Option<String>,
    /// Applicant/assignee of the cited patent. Useful for detecting
    /// self-citations (same name as the parent's applicants).
    pub name: Option<String>,
}

/// Citation directions for one patent. Returned by [`EpoClient::fetch_citations`].
///
/// EPO's biblio endpoint exposes only backward citations; `citing` is always
/// empty here. Use [`EpoClient::fetch_citing`] to populate forward refs via
/// a separate search call.
#[derive(Debug, Clone, serde::Serialize)]
#[non_exhaustive]
pub struct Citations {
    /// Backward citations — patents that THIS patent's application cites
    /// (submitted by applicant or examiner).
    pub cited: Vec<Citation>,
    /// Forward citations — patents that cite THIS patent. Empty from
    /// [`EpoClient::fetch_citations`]; populated by
    /// [`EpoClient::fetch_citing`].
    pub citing: Vec<Citation>,
}

/// Bibliographic metadata for one patent. Returned by [`EpoClient::fetch_biblio`].
///
/// All fields except `title`/`abstract_text`/`classification`/`cpc_classifications`
/// are `Option`/empty when EPO did not supply them. The parser prefers the
/// granted publication (kind starting with `B`) when EPO returns an
/// application+granted pair for the same patent.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct PatentBiblio {
    /// English-language invention title, falling back to the first available
    /// language when no English variant is present.
    pub title: String,
    /// Full English-language abstract body, extracted from the `<abstract><p>`
    /// shape. Empty when EPO didn't include an abstract (some applications
    /// ship without one).
    pub abstract_text: String,
    /// First applicant in epodoc format. Kept for backward compatibility; new
    /// callers should prefer [`applicants`](Self::applicants) for the full
    /// list. Equivalent to `applicants.first().cloned()`.
    pub assignee: Option<String>,
    /// All distinct epodoc-format applicants (or the original-format list as
    /// fallback). EPO often lists multiple — joint applicants, subsidiaries,
    /// translation variants — and each carries the country-code suffix
    /// (`[NL]`, `[US]`, …) when known.
    pub applicants: Vec<String>,
    /// Inventors in epodoc format. Same fallback as
    /// [`applicants`](Self::applicants) — original-format names are skipped
    /// when an epodoc entry exists.
    pub inventors: Vec<String>,
    /// Application filing date (`YYYY-MM-DD`).
    pub filing_date: Option<String>,
    /// Publication date (`YYYY-MM-DD`) of the chosen exchange-document. When
    /// EPO returns A1+B1 for the same patent, this is the B1 (granted) date.
    pub publication_date: Option<String>,
    /// Earliest priority claim date (`YYYY-MM-DD`). For PCT applications this
    /// is usually the original national filing.
    pub priority_date: Option<String>,
    /// Publication kind code: `A1`/`A2` (application), `B1`/`B2` (granted),
    /// `T1`/`T2`/`T3` (translations), country-specific.
    pub kind_code: Option<String>,
    /// EPO `@family-id`. Pass to [`EpoClient::fetch_family`] callers if you
    /// want to skip a round-trip — same family-id always yields the same
    /// member list.
    pub family_id: Option<String>,
    /// Full IPC codes (`B28B1/29`), de-duplicated across the
    /// `classification-ipc` and `classifications-ipcr` blocks. Not truncated
    /// to subclass.
    pub classification: Vec<String>,
    /// CPC codes from the structured `patent-classifications` block,
    /// de-duplicated across reporting offices (US/EP/IL/KR/CN can each
    /// classify the same patent with the same code).
    pub cpc_classifications: Vec<String>,
}

/// One result row from [`EpoClient::fetch_search`]. Mirrors [`PatentBiblio`]
/// but with the patent's own publication number as the primary key.
#[derive(Debug, Clone, serde::Serialize)]
#[non_exhaustive]
pub struct SearchResultPatent {
    /// Publication number in epodoc format.
    pub patent_id: String,
    /// English-language title, with fallback as in [`PatentBiblio::title`].
    pub title: String,
    /// English-language abstract body. Empty when EPO didn't include one.
    pub abstract_text: String,
    /// First epodoc applicant. Backward-compat shortcut for `applicants[0]`.
    pub assignee: Option<String>,
    /// All distinct epodoc-format applicants. See [`PatentBiblio::applicants`].
    pub applicants: Vec<String>,
    /// Inventors in epodoc format.
    pub inventors: Vec<String>,
    /// Application filing date (`YYYY-MM-DD`).
    pub filing_date: Option<String>,
    /// Publication date (`YYYY-MM-DD`).
    pub publication_date: Option<String>,
    /// Earliest priority claim date (`YYYY-MM-DD`).
    pub priority_date: Option<String>,
    /// Publication kind code (`A1`, `B1`, `T1`, …).
    pub kind_code: Option<String>,
    /// EPO `@family-id`.
    pub family_id: Option<String>,
    /// Full IPC codes (`B28B1/29`).
    pub classification: Vec<String>,
    /// CPC codes from `patent-classifications`.
    pub cpc_classifications: Vec<String>,
}

/// Full-text description for a published patent. Returned by
/// [`EpoClient::fetch_description`].
///
/// Description availability is patchy: many older publications and
/// non-major-jurisdiction filings are biblio-only in EPO's index, even
/// though `fetch_biblio` succeeds. A successful biblio fetch is **not** a
/// guarantee that the description endpoint will return 200.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct PatentDescription {
    /// The publication number that was queried.
    pub patent_id: String,
    /// Description language as reported by EPO (`@lang` attribute on
    /// `<description>`). Usually the original filing language. `None` when
    /// EPO didn't tag the response.
    pub language: Option<String>,
    /// Paragraph-level breakdown, preserving EPO's `@num` ordering tags
    /// (e.g. `"0001"`, `"0002"`, …) when present. Empty paragraphs are
    /// skipped.
    pub paragraphs: Vec<DescriptionParagraph>,
    /// Convenience field: all paragraph texts joined with `"\n\n"`. Use
    /// this when you want the description as a single blob for embedding
    /// or LLM context; use [`paragraphs`](Self::paragraphs) when you need
    /// per-paragraph addressing.
    pub plain_text: String,
}

/// One paragraph of a [`PatentDescription`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DescriptionParagraph {
    /// EPO's `@num` attribute on the `<p>` element — usually a 4-digit
    /// zero-padded sequence (`"0001"`). `None` when EPO didn't tag it.
    pub num: Option<String>,
    /// Paragraph body text, trimmed.
    pub text: String,
}

/// Full claim set for a published patent. Returned by
/// [`EpoClient::fetch_claims`].
///
/// Claims are what define infringement (descriptions are background); a
/// patent without claims is rare but possible (`fetch_claims` returns
/// [`EpoError::NotFound`] in that case).
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct PatentClaims {
    /// The publication number that was queried.
    pub patent_id: String,
    /// Claim set language as reported by EPO (`@lang` attribute).
    pub language: Option<String>,
    /// Individual claims in publication order. EPO's `@id`/`@num`
    /// attributes are preserved when present.
    pub claims: Vec<Claim>,
    /// Convenience field: all claim bodies joined with `"\n\n"`. Use
    /// [`claims`](Self::claims) when you need per-claim addressing
    /// (claim charts, dependency graphs).
    pub plain_text: String,
}

/// One claim from a [`PatentClaims`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Claim {
    /// EPO's `@num` attribute (e.g. `"0001"`, `"0002"`). `None` when
    /// EPO didn't tag it.
    pub num: Option<String>,
    /// EPO's `@id` attribute (e.g. `"claim001"`). `None` when absent.
    pub id: Option<String>,
    /// Claim body text. EPO sometimes wraps formatting (italics,
    /// subscripts, math) in nested elements — those are flattened to
    /// plain text here. If you need formatted output, parse the raw
    /// JSON yourself with [`parse_claims`].
    pub text: String,
}

/// Page of results from [`EpoClient::fetch_search`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SearchResults {
    /// Total number of patents matching the query across the whole index
    /// (NOT just the returned page). EPO caps very large queries.
    pub total_count: u32,
    /// `(begin, end)` of the returned slice (1-indexed, inclusive). For a
    /// `Range: 1-10` request returning 10 hits, this is `(1, 10)`.
    pub range: (u32, u32),
    /// The actual hits, deduplicated by `patent_id` within the page.
    pub patents: Vec<SearchResultPatent>,
}

struct TokenState {
    access_token: String,
    expires_at: std::time::Instant,
}

/// Per-endpoint min-interval pacer. Serializes callers via an async mutex
/// holding the last-call timestamp; each waiter sleeps until
/// `last_call + interval` before its turn.
struct EndpointPacer {
    last_call: Mutex<Option<std::time::Instant>>,
    interval: Duration,
}

pub struct EpoClient {
    http: reqwest::Client,
    consumer_key: String,
    consumer_secret: String,
    config: ClientConfig,
    token: Arc<Mutex<Option<TokenState>>>,
    /// Most recent throttling state from EPO response headers. `None` until
    /// the first successful HTTP call.
    throttling: Arc<std::sync::Mutex<Option<ThrottlingState>>>,
    /// Per-endpoint pacers. `inpadoc` covers biblio/family/citations,
    /// `search` covers search/citing, `retrieval` covers description/claims,
    /// `other` covers the auth + token refresh path.
    pacer_inpadoc: Arc<EndpointPacer>,
    pacer_search: Arc<EndpointPacer>,
    pacer_retrieval: Arc<EndpointPacer>,
    pacer_other: Arc<EndpointPacer>,
    /// Bounds in-flight requests across every endpoint. Built from
    /// [`ClientConfig::max_concurrent`]; the per-endpoint pacers run beneath
    /// it so a small permit count + tight pacers compose without conflict.
    sem: Arc<tokio::sync::Semaphore>,
}

/// Tunables for [`EpoClient`]. Use [`ClientConfig::default`] for sensible
/// defaults that match EPO's documented per-endpoint ceilings, or
/// [`EpoClient::with_config`] to override individual fields.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Base URL for the EPO OPS API. Default: `https://ops.epo.org/3.2`.
    pub base_url: String,
    /// Backoff schedule for retrying `403`/`429` errors. Total attempts =
    /// `retry_backoff.len()`. Element 0 is unused (the per-endpoint pacer
    /// already enforces baseline pacing on the first attempt); subsequent
    /// elements are the extra delay before each retry: `retry_backoff[1]`
    /// before attempt 1, `retry_backoff[2]` before attempt 2, …
    /// Default: `[250 ms, 500 ms, 1000 ms, 2000 ms]` — 4 attempts, with
    /// 500/1000/2000 ms extra spacing between retries (≈3.5 s aggregate).
    pub retry_backoff: Vec<Duration>,
    /// Buffer subtracted from the OAuth token's `expires_in` to trigger
    /// early refresh. Default: 60 s.
    pub token_refresh_buffer: Duration,
    /// Min interval between calls to *inpadoc* endpoints (biblio, family,
    /// citations). EPO's documented free-tier ceiling is 45 req/min →
    /// default ≈ 1.334 s/call. Lower for paid plans.
    pub inpadoc_interval: Duration,
    /// Min interval between calls to *search* endpoints (search, citing).
    /// EPO's documented free-tier ceiling is 15 req/min → default 4 s/call.
    /// Tightest of the four — search burns its budget fastest.
    pub search_interval: Duration,
    /// Min interval between calls to *retrieval* endpoints (description,
    /// claims). EPO's documented free-tier ceiling is 100 req/min →
    /// default 600 ms/call.
    pub retrieval_interval: Duration,
    /// Min interval for the *other* category (auth/status). 1000 req/min
    /// → default 60 ms/call. Mostly negligible since the token is cached.
    pub other_interval: Duration,
    /// Soft warning threshold for weekly bytes used (`x-registeredquotaperweek-used`).
    /// Logs a `warn!` when crossed. Default: 75% of 4 GB free-tier ceiling.
    pub weekly_warn_bytes: u64,
    /// Maximum in-flight requests across every endpoint. Acquired at the
    /// entry of each public `fetch_*` method via an internal semaphore,
    /// so callers don't have to wrap calls in their own concurrency
    /// guard. Composes with the per-endpoint pacers: e.g. `max_concurrent =
    /// 8` lets up to 8 requests overlap, each still waiting on its
    /// endpoint pacer before firing. `0` is clamped to `1` at construction
    /// (a zero-permit semaphore would deadlock the first call). Default: 8.
    pub max_concurrent: usize,
}

/// Overall server load reported in `x-throttling-control`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ThrottlingLoad {
    /// `idle` — normal operation.
    Idle,
    /// `busy` — under load but functional.
    Busy,
    /// `overloaded` — slow down voluntarily.
    Overloaded,
    /// `service_unavailable` — back off entirely.
    Unavailable,
    /// Unrecognised load string from EPO. Preserved verbatim so callers
    /// can log / surface the unknown value rather than silently flattening
    /// it. Example: a future `super_overloaded` state would land here.
    Other(String),
}

/// Per-endpoint quota color from `x-throttling-control`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ThrottlingColor {
    /// Plenty of headroom.
    Green,
    /// Slow down.
    Yellow,
    /// Close to limit.
    Red,
    /// Quota exhausted; requests will fail.
    Black,
}

/// Per-endpoint slice of [`ThrottlingState`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct EndpointQuota {
    /// `green`/`yellow`/`red`/`black` from EPO.
    pub color: ThrottlingColor,
    /// Requests-per-minute remaining at this color level. EPO's number;
    /// not adjusted for any in-flight requests on our side.
    pub remaining_per_minute: u32,
}

/// Snapshot of the EPO throttling + quota state, parsed from the response
/// headers of the most recent successful HTTP call. Returned by
/// [`EpoClient::throttling_state`].
///
/// Updated after every successful response (token + data calls). Never
/// stale by more than one round-trip; never updated on errors that
/// returned no headers.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ThrottlingState {
    /// Overall load level (`idle`/`busy`/`overloaded`/`service_unavailable`).
    pub load: ThrottlingLoad,
    /// Per-endpoint allowances. Keys are EPO's canonical names: `images`,
    /// `inpadoc`, `other`, `retrieval`, `search`. Endpoints absent from
    /// the response header are absent from this map.
    pub endpoints: std::collections::HashMap<String, EndpointQuota>,
    /// `x-individualquotaperhour-used` — bytes pulled this hour.
    pub hour_bytes_used: Option<u64>,
    /// `x-registeredquotaperweek-used` — bytes pulled this week. The
    /// free-tier weekly ceiling is 4 GB.
    pub week_bytes_used: Option<u64>,
}

impl ThrottlingState {
    /// `true` if any endpoint quota slot is unusable: either color is
    /// `black` (EPO has marked it dead), or `remaining_per_minute == 0`
    /// regardless of color (e.g. `green:0` — sometimes EPO surfaces this
    /// transiently). Callers doing batch pre-flight should refuse new
    /// work in either case; the next request would 403 anyway.
    pub fn is_exhausted(&self) -> bool {
        self.endpoints
            .values()
            .any(|q| q.color == ThrottlingColor::Black || q.remaining_per_minute == 0)
    }

    /// Convenience accessor: remaining slots for the `inpadoc` endpoint
    /// group (biblio / family / citations).
    pub fn inpadoc_remaining(&self) -> Option<u32> {
        self.endpoints
            .get("inpadoc")
            .map(|q| q.remaining_per_minute)
    }

    /// Convenience accessor: remaining slots for the `search` endpoint
    /// group (search / citing).
    pub fn search_remaining(&self) -> Option<u32> {
        self.endpoints.get("search").map(|q| q.remaining_per_minute)
    }

    /// Convenience accessor: remaining slots for the `retrieval` endpoint
    /// group (description / claims).
    pub fn retrieval_remaining(&self) -> Option<u32> {
        self.endpoints
            .get("retrieval")
            .map(|q| q.remaining_per_minute)
    }
}

/// `expires_in` is what EPO reports; subtract `buffer` and clamp to zero so
/// a tiny EPO TTL plus a too-large buffer doesn't underflow.
fn expires_in_with_buffer(expires_in: u64, buffer: Duration) -> Duration {
    Duration::from_secs(expires_in.saturating_sub(buffer.as_secs()))
}

/// Compare new throttling state against previous, emit `warn!` on the
/// transitions worth waking up an operator for.
fn emit_threshold_warnings(
    prev: Option<&ThrottlingState>,
    new: &ThrottlingState,
    weekly_warn_bytes: u64,
) {
    // Load level transition (idle → busy → overloaded → unavailable).
    let prev_load = prev.map(|p| &p.load);
    if prev_load != Some(&new.load) && new.load != ThrottlingLoad::Idle {
        warn!(load = ?new.load, "EPO server load level changed");
    }

    // Per-endpoint color worsening.
    for (name, quota) in &new.endpoints {
        let prev_color = prev.and_then(|p| p.endpoints.get(name)).map(|q| q.color);
        let worsened = matches!(
            (prev_color, quota.color),
            (
                Some(ThrottlingColor::Green),
                ThrottlingColor::Yellow | ThrottlingColor::Red | ThrottlingColor::Black
            ) | (
                Some(ThrottlingColor::Yellow),
                ThrottlingColor::Red | ThrottlingColor::Black
            ) | (Some(ThrottlingColor::Red), ThrottlingColor::Black)
                | (
                    None,
                    ThrottlingColor::Yellow | ThrottlingColor::Red | ThrottlingColor::Black
                )
        );
        if worsened {
            warn!(
                endpoint = %name,
                color = ?quota.color,
                remaining = quota.remaining_per_minute,
                "EPO endpoint quota worsened"
            );
        }
    }

    // Weekly bytes crossing the warn threshold (rising edge only).
    let prev_week = prev.and_then(|p| p.week_bytes_used).unwrap_or(0);
    if let Some(now_week) = new.week_bytes_used
        && prev_week < weekly_warn_bytes
        && now_week >= weekly_warn_bytes
    {
        warn!(
            bytes_used = now_week,
            threshold = weekly_warn_bytes,
            "EPO weekly quota crossed warn threshold"
        );
    }
}

/// Parse EPO's `x-throttling-control` header into a [`ThrottlingState`].
///
/// Format: `<load> (<endpoint>=<color>:<remaining>, ...)`. Examples:
///
/// ```text
/// idle (images=green:200, inpadoc=green:60, retrieval=green:200, search=green:30)
/// busy (images=green:100, inpadoc=yellow:30, search=red:5)
/// overloaded (search=black:0)
/// ```
///
/// `hour_used` and `week_used` are filled by the caller from the
/// `x-individualquotaperhour-used` / `x-registeredquotaperweek-used`
/// headers. Returns `None` when the header is malformed beyond recognition;
/// callers treat that as "no update".
pub(crate) fn parse_throttling_header(
    header: &str,
    hour_used: Option<u64>,
    week_used: Option<u64>,
) -> Option<ThrottlingState> {
    let header = header.trim();
    let (load_str, rest) = match header.split_once('(') {
        Some((load, rest)) => (load.trim(), rest.trim_end_matches(')').trim()),
        None => (header, ""),
    };

    let load = match load_str.to_ascii_lowercase().as_str() {
        "idle" => ThrottlingLoad::Idle,
        "busy" => ThrottlingLoad::Busy,
        "overloaded" => ThrottlingLoad::Overloaded,
        "service_unavailable" => ThrottlingLoad::Unavailable,
        _ => ThrottlingLoad::Other(load_str.to_string()),
    };

    let mut endpoints = std::collections::HashMap::new();
    for entry in rest.split(',') {
        let entry = entry.trim();
        let Some((name, rest)) = entry.split_once('=') else {
            continue;
        };
        let Some((color_str, remaining_str)) = rest.split_once(':') else {
            continue;
        };
        let color = match color_str.to_ascii_lowercase().as_str() {
            "green" => ThrottlingColor::Green,
            "yellow" => ThrottlingColor::Yellow,
            "red" => ThrottlingColor::Red,
            "black" => ThrottlingColor::Black,
            _ => continue,
        };
        let Ok(remaining) = remaining_str.trim().parse::<u32>() else {
            continue;
        };
        endpoints.insert(
            name.trim().to_ascii_lowercase(),
            EndpointQuota {
                color,
                remaining_per_minute: remaining,
            },
        );
    }

    Some(ThrottlingState {
        load,
        endpoints,
        hour_bytes_used: hour_used,
        week_bytes_used: week_used,
    })
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_EPO_BASE.to_string(),
            retry_backoff: vec![
                Duration::from_millis(250),
                Duration::from_millis(500),
                Duration::from_millis(1000),
                Duration::from_millis(2000),
            ],
            token_refresh_buffer: Duration::from_secs(60),
            // Defaults derived from EPO OPS free-tier per-minute ceilings:
            // inpadoc=45 → 1334ms, search=15 → 4000ms,
            // retrieval=100 → 600ms, other=1000 → 60ms.
            inpadoc_interval: Duration::from_millis(1334),
            search_interval: Duration::from_millis(4000),
            retrieval_interval: Duration::from_millis(600),
            other_interval: Duration::from_millis(60),
            // 75% of 4 GB free-tier weekly ceiling.
            weekly_warn_bytes: (4u64 * 1024 * 1024 * 1024) * 3 / 4,
            // 8 in-flight requests is the sweet spot for the free-tier
            // pacers: enough overlap that I/O dominates, not so many that
            // the pacers serialise into a single-file queue.
            max_concurrent: 8,
        }
    }
}

/// Shared retry-with-pacer wrapper. Each attempt waits for the per-endpoint
/// pacer before firing; failed attempts that returned [`EpoError::RateLimit`]
/// add an extra sleep from `backoff` (skipping `backoff[0]` since the pacer
/// already enforces baseline pacing). Stops after `backoff.len()` attempts.
async fn retry_with_pacer<F, Fut, T>(
    pacer: &EndpointPacer,
    backoff: &[Duration],
    patent_id: &str,
    endpoint: &str,
    mut op: F,
) -> Result<T, EpoError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, EpoError>>,
{
    let attempts = backoff.len().max(1);
    let span = Span::current();
    for attempt in 0..attempts {
        if attempt > 0
            && let Some(delay) = backoff.get(attempt)
        {
            sleep(*delay).await;
        }
        pacer.wait_turn().await;
        match op().await {
            Ok(v) => {
                span.record("attempts", attempt as u64 + 1);
                return Ok(v);
            }
            Err(EpoError::RateLimit) if attempt + 1 < attempts => {
                warn!(
                    endpoint,
                    patent_id,
                    attempt = attempt + 1,
                    "EPO rate limit, will retry"
                );
            }
            Err(e) => {
                span.record("attempts", attempt as u64 + 1);
                return Err(e);
            }
        }
    }
    span.record("attempts", attempts as u64);
    Err(EpoError::RateLimit)
}

impl EndpointPacer {
    fn new(interval: Duration) -> Self {
        Self {
            last_call: Mutex::new(None),
            interval,
        }
    }

    async fn wait_turn(&self) {
        if self.interval.is_zero() {
            return;
        }
        let mut g = self.last_call.lock().await;
        if let Some(last) = *g {
            let elapsed = std::time::Instant::now().saturating_duration_since(last);
            if elapsed < self.interval {
                sleep(self.interval - elapsed).await;
            }
        }
        *g = Some(std::time::Instant::now());
    }
}

impl EpoClient {
    /// Construct a client with default tunables. `base_url` defaults to
    /// `https://ops.epo.org/3.2` when `None`.
    pub fn new(consumer_key: String, consumer_secret: String, base_url: Option<String>) -> Self {
        let mut config = ClientConfig::default();
        if let Some(url) = base_url {
            config.base_url = url;
        }
        Self::with_config(consumer_key, consumer_secret, config)
    }

    /// Construct a client with explicit [`ClientConfig`]. Use this to tune
    /// retry/backoff for your own quotas, point at a mock server, or shorten
    /// the token-refresh buffer for tests.
    pub fn with_config(
        consumer_key: String,
        consumer_secret: String,
        config: ClientConfig,
    ) -> Self {
        let pacer_inpadoc = Arc::new(EndpointPacer::new(config.inpadoc_interval));
        let pacer_search = Arc::new(EndpointPacer::new(config.search_interval));
        let pacer_retrieval = Arc::new(EndpointPacer::new(config.retrieval_interval));
        let pacer_other = Arc::new(EndpointPacer::new(config.other_interval));
        // Zero-permit semaphore would deadlock the first `acquire`; clamp.
        let sem = Arc::new(tokio::sync::Semaphore::new(config.max_concurrent.max(1)));
        Self {
            http: reqwest::Client::new(),
            consumer_key,
            consumer_secret,
            config,
            token: Arc::new(Mutex::new(None)),
            throttling: Arc::new(std::sync::Mutex::new(None)),
            pacer_inpadoc,
            pacer_search,
            pacer_retrieval,
            pacer_other,
            sem,
        }
    }

    /// Snapshot of EPO's throttling + quota state from the most recent
    /// successful response. `None` until the first call completes.
    ///
    /// Updated after every successful HTTP round-trip; never updated on
    /// errors that returned no headers. Use this for batch pre-flight
    /// (refuse new work when [`ThrottlingState::is_exhausted`]) and for
    /// metrics dashboards.
    pub fn throttling_state(&self) -> Option<ThrottlingState> {
        self.throttling.lock().ok().and_then(|g| g.clone())
    }

    /// Extract throttling/quota headers from a response and update the
    /// snapshot. Logs `warn!` on transitions that callers should care
    /// about: load going non-idle, any endpoint going non-green, weekly
    /// bytes crossing [`ClientConfig::weekly_warn_bytes`].
    fn update_throttling(&self, headers: &reqwest::header::HeaderMap) {
        let header_value = headers
            .get("x-throttling-control")
            .and_then(|v| v.to_str().ok());
        let hour = headers
            .get("x-individualquotaperhour-used")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok());
        let week = headers
            .get("x-registeredquotaperweek-used")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok());

        let Some(throttling_str) = header_value else {
            return;
        };
        let Some(new_state) = parse_throttling_header(throttling_str, hour, week) else {
            return;
        };

        let mut g = match self.throttling.lock() {
            Ok(g) => g,
            Err(_) => return, // poisoned lock, skip silently
        };
        let prev = g.clone();
        emit_threshold_warnings(prev.as_ref(), &new_state, self.config.weekly_warn_bytes);
        *g = Some(new_state);
    }

    async fn get_token(&self) -> Result<String, EpoError> {
        let mut guard = self.token.lock().await;

        // Return cached token if still valid
        if let Some(ref state) = *guard
            && state.expires_at > std::time::Instant::now()
        {
            return Ok(state.access_token.clone());
        }

        // Token endpoint counts as `other` quota — pace independently.
        self.pacer_other.wait_turn().await;

        // Fetch new token
        let resp = self
            .http
            .post(format!("{}/auth/accesstoken", self.config.base_url))
            .basic_auth(&self.consumer_key, Some(&self.consumer_secret))
            .form(&[("grant_type", "client_credentials")])
            .send()
            .await
            .map_err(|e| EpoError::Auth(e.to_string()))?;

        // EPO sets throttling/quota headers on auth-error responses too —
        // auth counts against the `other` quota — so capture before any
        // early-return to keep the snapshot fresh on 4xx.
        self.update_throttling(resp.headers());

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(EpoError::Auth(format!("{status}: {body}")));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| EpoError::Auth(e.to_string()))?;

        let access_token = json["access_token"]
            .as_str()
            .ok_or_else(|| EpoError::Auth("No access_token in response".into()))?
            .to_string();

        let expires_in = json["expires_in"]
            .as_u64()
            .or_else(|| json["expires_in"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(1200); // default 20 min

        let expires_at = std::time::Instant::now()
            + expires_in_with_buffer(expires_in, self.config.token_refresh_buffer);

        *guard = Some(TokenState {
            access_token: access_token.clone(),
            expires_at,
        });

        Ok(access_token)
    }

    #[instrument(skip(self), fields(endpoint = "biblio", attempts = field::Empty))]
    pub async fn fetch_biblio(&self, patent_id: &str) -> Result<PatentBiblio, EpoError> {
        let _permit = self.sem.acquire().await.expect("EPO semaphore poisoned");
        retry_with_pacer(
            &self.pacer_inpadoc,
            &self.config.retry_backoff,
            patent_id,
            "biblio",
            || self.fetch_biblio_once(patent_id),
        )
        .await
    }

    async fn fetch_biblio_once(&self, patent_id: &str) -> Result<PatentBiblio, EpoError> {
        Ok(parse_biblio(&self.fetch_biblio_json_once(patent_id).await?))
    }

    /// One-shot HTTP call to the biblio endpoint, returning the raw JSON.
    /// Shared by [`fetch_biblio`](Self::fetch_biblio),
    /// [`fetch_citations`](Self::fetch_citations), and
    /// [`fetch_biblio_with_citations`](Self::fetch_biblio_with_citations) so
    /// callers that need both biblio + citations only pay one round-trip.
    async fn fetch_biblio_json_once(&self, patent_id: &str) -> Result<serde_json::Value, EpoError> {
        let token = self.get_token().await?;

        let url = format!(
            "{}/rest-services/published-data/publication/epodoc/{patent_id}/biblio",
            self.config.base_url
        );

        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| EpoError::Network(e.to_string()))?;

        // Update before the status match so 403/429 responses (which carry
        // fresh throttling headers — exactly when callers most need them)
        // refresh the snapshot before we early-return.
        self.update_throttling(resp.headers());

        match resp.status().as_u16() {
            200 => {}
            404 => return Err(EpoError::NotFound),
            403 | 429 => return Err(EpoError::RateLimit),
            status => {
                let body = resp.text().await.unwrap_or_default();
                return Err(EpoError::Api(format!("{status}: {body}")));
            }
        }

        resp.json::<serde_json::Value>()
            .await
            .map_err(|e| EpoError::Api(e.to_string()))
    }

    /// Fetch biblio + citations in a single HTTP round-trip.
    ///
    /// EPO's biblio endpoint already includes the `references-cited` block,
    /// so calling [`fetch_biblio`](Self::fetch_biblio) followed by
    /// [`fetch_citations`](Self::fetch_citations) doubles the request count
    /// (and the quota cost) for no benefit. Use this whenever you need
    /// both. Same retry/backoff schedule as [`fetch_biblio`](Self::fetch_biblio).
    #[instrument(skip(self), fields(endpoint = "biblio+citations", attempts = field::Empty))]
    pub async fn fetch_biblio_with_citations(
        &self,
        patent_id: &str,
    ) -> Result<(PatentBiblio, Citations), EpoError> {
        let _permit = self.sem.acquire().await.expect("EPO semaphore poisoned");
        let json = retry_with_pacer(
            &self.pacer_inpadoc,
            &self.config.retry_backoff,
            patent_id,
            "biblio+citations",
            || self.fetch_biblio_json_once(patent_id),
        )
        .await?;
        let biblio = parse_biblio(&json);
        let citations = parse_citations(&json);
        debug!(
            patent_id,
            cited = citations.cited.len(),
            "EPO biblio+citations fetch succeeded"
        );
        Ok((biblio, citations))
    }

    #[instrument(skip(self), fields(endpoint = "citations", attempts = field::Empty))]
    pub async fn fetch_citations(&self, patent_id: &str) -> Result<Citations, EpoError> {
        let _permit = self.sem.acquire().await.expect("EPO semaphore poisoned");
        let json = retry_with_pacer(
            &self.pacer_inpadoc,
            &self.config.retry_backoff,
            patent_id,
            "citations",
            || self.fetch_biblio_json_once(patent_id),
        )
        .await?;
        Ok(parse_citations(&json))
    }

    #[instrument(skip(self, cql), fields(
        endpoint = "search",
        query = %cql,
        range_begin,
        range_end,
        http_status = field::Empty,
        total_count = field::Empty,
        returned = field::Empty,
        attempts = field::Empty,
    ))]
    pub async fn fetch_search(
        &self,
        cql: &str,
        range_begin: u32,
        range_end: u32,
    ) -> Result<SearchResults, EpoError> {
        let _permit = self.sem.acquire().await.expect("EPO semaphore poisoned");
        let val = retry_with_pacer(
            &self.pacer_search,
            &self.config.retry_backoff,
            cql,
            "search",
            || self.fetch_search_once(cql, range_begin, range_end),
        )
        .await?;
        let span = Span::current();
        span.record("total_count", val.total_count);
        span.record("returned", val.patents.len());
        info!("EPO search fetch succeeded");
        Ok(val)
    }

    async fn fetch_search_once(
        &self,
        cql: &str,
        range_begin: u32,
        range_end: u32,
    ) -> Result<SearchResults, EpoError> {
        let token = self.get_token().await?;

        let url = format!(
            "{}/rest-services/published-data/search/biblio",
            self.config.base_url
        );

        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .bearer_auth(&token)
            .query(&[("q", cql), ("Range", &format!("{range_begin}-{range_end}"))])
            .send()
            .await
            .map_err(|e| EpoError::Network(e.to_string()))?;

        let status = resp.status().as_u16();
        Span::current().record("http_status", status);
        self.update_throttling(resp.headers());

        match status {
            200 => {}
            404 => {
                return Ok(SearchResults {
                    total_count: 0,
                    range: (0, 0),
                    patents: Vec::new(),
                });
            }
            403 | 429 => return Err(EpoError::RateLimit),
            other => {
                let body = resp.text().await.unwrap_or_default();
                return Err(EpoError::Api(format!("{other}: {body}")));
            }
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| EpoError::Api(e.to_string()))?;

        Ok(parse_search_results(&json))
    }

    #[instrument(skip(self), fields(endpoint = "family", attempts = field::Empty))]
    pub async fn fetch_family(&self, patent_id: &str) -> Result<Vec<FamilyMember>, EpoError> {
        let _permit = self.sem.acquire().await.expect("EPO semaphore poisoned");
        retry_with_pacer(
            &self.pacer_inpadoc,
            &self.config.retry_backoff,
            patent_id,
            "family",
            || self.fetch_family_once(patent_id),
        )
        .await
    }

    async fn fetch_family_once(&self, patent_id: &str) -> Result<Vec<FamilyMember>, EpoError> {
        let token = self.get_token().await?;

        let url = format!(
            "{}/rest-services/family/publication/epodoc/{patent_id}",
            self.config.base_url
        );

        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| EpoError::Network(e.to_string()))?;

        self.update_throttling(resp.headers());

        match resp.status().as_u16() {
            200 => {}
            404 => return Err(EpoError::NotFound),
            403 | 429 => return Err(EpoError::RateLimit),
            status => {
                let body = resp.text().await.unwrap_or_default();
                return Err(EpoError::Api(format!("{status}: {body}")));
            }
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| EpoError::Api(e.to_string()))?;

        let family = parse_family(&json);
        debug!(
            patent_id,
            members = family.len(),
            "EPO family fetch succeeded"
        );
        Ok(family)
    }

    /// Fetch the full-text description body for a published patent.
    ///
    /// Description availability depends on jurisdiction and language —
    /// many older publications are biblio-only in EPO's index. A patent
    /// that succeeds on [`fetch_biblio`](Self::fetch_biblio) may still
    /// return [`EpoError::NotFound`] here.
    ///
    /// Counts against EPO's "retrieval" quota, which is separate from
    /// biblio. Honours [`ClientConfig::retrieval_interval`] pacing and
    /// retry-on-403/429 schedule.
    #[instrument(skip(self), fields(endpoint = "description", attempts = field::Empty))]
    pub async fn fetch_description(&self, patent_id: &str) -> Result<PatentDescription, EpoError> {
        let _permit = self.sem.acquire().await.expect("EPO semaphore poisoned");
        retry_with_pacer(
            &self.pacer_retrieval,
            &self.config.retry_backoff,
            patent_id,
            "description",
            || self.fetch_description_once(patent_id),
        )
        .await
    }

    async fn fetch_description_once(&self, patent_id: &str) -> Result<PatentDescription, EpoError> {
        let token = self.get_token().await?;
        let url = format!(
            "{}/rest-services/published-data/publication/epodoc/{patent_id}/description",
            self.config.base_url
        );
        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| EpoError::Network(e.to_string()))?;

        self.update_throttling(resp.headers());

        match resp.status().as_u16() {
            200 => {}
            404 => return Err(EpoError::NotFound),
            403 | 429 => return Err(EpoError::RateLimit),
            status => {
                let body = resp.text().await.unwrap_or_default();
                return Err(EpoError::Api(format!("{status}: {body}")));
            }
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| EpoError::Api(e.to_string()))?;
        Ok(parse_description(&json, patent_id))
    }

    /// Fetch the claim set for a published patent.
    ///
    /// Counts against EPO's "retrieval" quota (separate from biblio).
    /// Claim availability mirrors descriptions — major-jurisdiction recent
    /// publications usually have them; older or smaller-jurisdiction
    /// filings may 404 here even when [`fetch_biblio`](Self::fetch_biblio)
    /// works.
    ///
    /// EPO sometimes wraps inline formatting (italics, subscripts, math
    /// formulas) inside claim text. The default parse flattens them; if
    /// you need to preserve formatting, use [`parse_claims`] on the raw
    /// JSON yourself.
    #[instrument(skip(self), fields(endpoint = "claims", attempts = field::Empty))]
    pub async fn fetch_claims(&self, patent_id: &str) -> Result<PatentClaims, EpoError> {
        let _permit = self.sem.acquire().await.expect("EPO semaphore poisoned");
        retry_with_pacer(
            &self.pacer_retrieval,
            &self.config.retry_backoff,
            patent_id,
            "claims",
            || self.fetch_claims_once(patent_id),
        )
        .await
    }

    async fn fetch_claims_once(&self, patent_id: &str) -> Result<PatentClaims, EpoError> {
        let token = self.get_token().await?;
        let url = format!(
            "{}/rest-services/published-data/publication/epodoc/{patent_id}/claims",
            self.config.base_url
        );
        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| EpoError::Network(e.to_string()))?;

        self.update_throttling(resp.headers());

        match resp.status().as_u16() {
            200 => {}
            404 => return Err(EpoError::NotFound),
            403 | 429 => return Err(EpoError::RateLimit),
            status => {
                let body = resp.text().await.unwrap_or_default();
                return Err(EpoError::Api(format!("{status}: {body}")));
            }
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| EpoError::Api(e.to_string()))?;
        Ok(parse_claims(&json, patent_id))
    }

    /// Fetch forward citations: the patents that cite `patent_id`.
    ///
    /// EPO's biblio endpoint exposes only backward citations (the references
    /// the applicant or examiner submitted). Forward citations require a
    /// separate search. This helper runs a CQL `ct=<patent_id>` against the
    /// search endpoint and maps each hit to a [`Citation`] with `phase` set
    /// to `"citing"` so callers can distinguish them from backward refs.
    ///
    /// `max` caps the number of returned citations (EPO's range is 1-based,
    /// `max` becomes the upper bound). Realistic ceiling is 100.
    #[instrument(skip(self), fields(endpoint = "citing"))]
    pub async fn fetch_citing(&self, patent_id: &str, max: u32) -> Result<Vec<Citation>, EpoError> {
        let cql = format!("ct={patent_id}");
        let results = self.fetch_search(&cql, 1, max.max(1)).await?;
        Ok(results
            .patents
            .into_iter()
            .map(|p| Citation {
                patent_id: p.patent_id,
                phase: "citing".to_string(),
                category: None,
                cited_by: None,
                date: p.publication_date,
                name: p.assignee,
            })
            .collect())
    }

    /// Fan out per-member [`fetch_biblio`](Self::fetch_biblio) calls to fill
    /// in the `title` field that EPO's family endpoint omits.
    ///
    /// Costs N additional biblio requests for N family members, paced by
    /// the client's [`ClientConfig::inpadoc_interval`]. For a family of 6,
    /// expect ~8 s under default config (1.334 s × 6 paced calls).
    ///
    /// Members with a non-empty title are skipped. On per-member fetch
    /// errors the title stays empty (the function logs at debug level and
    /// continues). Returns the number of titles successfully filled.
    pub async fn enrich_family_titles(&self, family: &mut [FamilyMember]) -> usize {
        let mut filled = 0usize;
        for member in family.iter_mut() {
            if !member.title.is_empty() {
                continue;
            }
            match self.fetch_biblio(&member.patent_id).await {
                Ok(b) if !b.title.is_empty() => {
                    member.title = b.title;
                    filled += 1;
                }
                Ok(_) => {}
                Err(e) => {
                    debug!(
                        patent_id = %member.patent_id,
                        error = %e,
                        "enrich_family_titles: per-member biblio fetch failed"
                    );
                }
            }
        }
        filled
    }
}

/// Extract bibliographic data from EPO JSON response.
///
/// EPO returns multiple `exchange-document` entries per publication — one per
/// kind code (A1 = application, B1 = granted, …). We prefer B-kind (granted is
/// canonical) over A-kind, then any remaining doc. Fields like the abstract
/// fall back across documents when the chosen one is missing them, since EPO
/// sometimes omits a field on B1 that A1 had.
pub fn parse_biblio(json: &serde_json::Value) -> PatentBiblio {
    let docs = locate_exchange_docs(json);
    let chosen = match pick_preferred_doc(&docs) {
        Some(d) => d,
        None => return PatentBiblio::default(),
    };

    let biblio = &chosen["bibliographic-data"];
    let applicants = extract_applicants_all(biblio);
    let assignee = applicants.first().cloned();

    PatentBiblio {
        title: extract_text_by_lang(&biblio["invention-title"]),
        abstract_text: extract_abstract_with_fallback(chosen, &docs),
        assignee,
        applicants,
        inventors: extract_inventors(biblio),
        filing_date: extract_date(&biblio["application-reference"]["document-id"]),
        publication_date: extract_date(&biblio["publication-reference"]["document-id"]),
        priority_date: extract_priority_date(biblio),
        kind_code: chosen["@kind"].as_str().map(str::to_string),
        family_id: chosen["@family-id"].as_str().map(str::to_string),
        classification: extract_classifications(biblio),
        cpc_classifications: extract_cpc_classifications(biblio),
    }
}

/// Walk both the biblio (`ops:world-patent-data.exchange-documents.exchange-document`)
/// and search (`ops:world-patent-data.ops:biblio-search.ops:search-result.exchange-documents.exchange-document`)
/// shapes; the field can be a single object or array.
fn locate_exchange_docs(json: &serde_json::Value) -> Vec<&serde_json::Value> {
    let mut candidate = &json["ops:world-patent-data"]["exchange-documents"]["exchange-document"];
    if candidate.is_null() {
        candidate = &json["ops:world-patent-data"]["ops:biblio-search"]["ops:search-result"]["exchange-documents"]
            ["exchange-document"];
    }
    if let Some(arr) = candidate.as_array() {
        arr.iter().collect()
    } else if candidate.is_null() {
        Vec::new()
    } else {
        vec![candidate]
    }
}

/// Prefer B-kind (granted) over A-kind (application), else first available.
fn pick_preferred_doc<'a>(docs: &[&'a serde_json::Value]) -> Option<&'a serde_json::Value> {
    let by_b = docs
        .iter()
        .find(|d| d["@kind"].as_str().is_some_and(|k| k.starts_with('B')));
    if let Some(d) = by_b {
        return Some(*d);
    }
    let by_a = docs
        .iter()
        .find(|d| d["@kind"].as_str().is_some_and(|k| k.starts_with('A')));
    if let Some(d) = by_a {
        return Some(*d);
    }
    docs.first().copied()
}

fn extract_abstract_with_fallback(
    chosen: &serde_json::Value,
    all: &[&serde_json::Value],
) -> String {
    let primary = extract_text_by_lang(&chosen["abstract"]);
    if !primary.is_empty() {
        return primary;
    }
    for d in all {
        let txt = extract_text_by_lang(&d["abstract"]);
        if !txt.is_empty() {
            return txt;
        }
    }
    String::new()
}

/// Parse search results from EPO OPS biblio-search response.
///
/// EPO returns two distinct shapes here:
///   * `ops:range` lives on `ops:biblio-search` (NOT on `ops:search-result`),
///     and `@total-result-count` lives there too.
///   * `exchange-documents` in a search response is an **array of
///     `{ "exchange-document": {…} }` wrappers**, one per result — different
///     from the biblio response, where `exchange-documents` is a single
///     object containing `exchange-document: [array]`. We accept both
///     shapes so the same parser works for biblio-shaped fixtures and
///     real search responses.
#[instrument(skip(json), fields(
    doc_count = field::Empty,
    parsed = field::Empty,
    duplicates = field::Empty,
    malformed = field::Empty,
))]
pub fn parse_search_results(json: &serde_json::Value) -> SearchResults {
    let biblio_search = &json["ops:world-patent-data"]["ops:biblio-search"];
    let search_result = &biblio_search["ops:search-result"];

    if search_result.is_null() {
        return SearchResults {
            total_count: 0,
            range: (0, 0),
            patents: Vec::new(),
        };
    }

    let total_count = biblio_search["@total-result-count"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| biblio_search["@total-result-count"].as_u64())
        .unwrap_or(0) as u32;

    // `ops:range` is on biblio-search, not search-result. Tolerate the
    // alternative location so older fixtures still parse.
    let range_obj = if biblio_search["ops:range"].is_object() {
        &biblio_search["ops:range"]
    } else {
        &search_result["ops:range"]
    };
    let range_begin = range_obj["@begin"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let range_end = range_obj["@end"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let doc_refs = collect_search_docs(search_result);

    let mut patents = Vec::new();
    let mut seen = HashSet::new();
    let mut duplicates: u64 = 0;
    let mut malformed: u64 = 0;
    let doc_count = doc_refs.len() as u64;

    for doc in &doc_refs {
        let biblio = &doc["bibliographic-data"];

        let pub_ref = &biblio["publication-reference"]["document-id"];
        let patent_id = extract_patent_id_from_doc_ids(pub_ref);

        if patent_id.is_empty() {
            malformed += 1;
            debug!(
                pub_ref = %pub_ref,
                "EPO search doc missing patent_id"
            );
            continue;
        }
        if seen.contains(&patent_id) {
            duplicates += 1;
            continue;
        }
        seen.insert(patent_id.clone());

        let applicants = extract_applicants_all(biblio);
        let assignee = applicants.first().cloned();
        patents.push(SearchResultPatent {
            patent_id,
            title: extract_text_by_lang(&biblio["invention-title"]),
            abstract_text: extract_text_by_lang(&doc["abstract"]),
            assignee,
            applicants,
            inventors: extract_inventors(biblio),
            filing_date: extract_date(&biblio["application-reference"]["document-id"]),
            publication_date: extract_date(&biblio["publication-reference"]["document-id"]),
            priority_date: extract_priority_date(biblio),
            kind_code: doc["@kind"].as_str().map(str::to_string),
            family_id: doc["@family-id"].as_str().map(str::to_string),
            classification: extract_classifications(biblio),
            cpc_classifications: extract_cpc_classifications(biblio),
        });
    }

    let span = Span::current();
    span.record("doc_count", doc_count);
    span.record("parsed", patents.len() as u64);
    span.record("duplicates", duplicates);
    span.record("malformed", malformed);
    if duplicates > 0 || malformed > 0 {
        debug!(
            parsed = patents.len(),
            duplicates, malformed, "EPO search parse dropped entries"
        );
    }

    SearchResults {
        total_count,
        range: (range_begin, range_end),
        patents,
    }
}

/// Walk the `exchange-documents` field of a search response.
///
/// Real EPO search responses use `exchange-documents: [{exchange-document: {…}}, …]`
/// — an array where each element wraps a single doc. Some test fixtures and
/// older EPO responses use the biblio-style `exchange-documents: {exchange-document: […]}`.
/// This helper handles both shapes (and the single-doc-as-object variant)
/// so the parser doesn't silently drop results.
fn collect_search_docs(search_result: &serde_json::Value) -> Vec<&serde_json::Value> {
    let docs_field = &search_result["exchange-documents"];

    if let Some(arr) = docs_field.as_array() {
        // Real search shape: array of `{exchange-document: {…}}` wrappers.
        // Tolerate elements that are docs directly (no wrapper) for fixture flex.
        return arr
            .iter()
            .map(|item| {
                let nested = &item["exchange-document"];
                if nested.is_null() { item } else { nested }
            })
            .filter(|d| !d.is_null() && d.is_object())
            .collect();
    }

    // Biblio-style shape: `exchange-documents: {exchange-document: [array | object]}`.
    let nested = &docs_field["exchange-document"];
    if let Some(arr) = nested.as_array() {
        arr.iter().collect()
    } else if !nested.is_null() {
        vec![nested]
    } else {
        Vec::new()
    }
}

/// Extract patent ID from document-id array, preferring epodoc format.
fn extract_patent_id_from_doc_ids(doc_ids: &serde_json::Value) -> String {
    if doc_ids.is_null() {
        return String::new();
    }

    let items = if doc_ids.is_array() {
        doc_ids.as_array().unwrap().as_slice()
    } else {
        std::slice::from_ref(doc_ids)
    };

    // Prefer epodoc
    for item in items {
        let doc_type = item["@document-id-type"].as_str().unwrap_or("");
        if doc_type == "epodoc"
            && let Some(num) = item["doc-number"]["$"].as_str()
            && !num.is_empty()
        {
            return num.to_string();
        }
    }

    // Fall back to docdb (country + doc-number)
    for item in items {
        let doc_type = item["@document-id-type"].as_str().unwrap_or("");
        if doc_type == "docdb" {
            let country = item["country"]["$"].as_str().unwrap_or("");
            let num = item["doc-number"]["$"].as_str().unwrap_or("");
            if !num.is_empty() {
                return format!("{country}{num}");
            }
        }
    }

    String::new()
}

/// Extract text preferring English from EPO's lang-tagged objects.
/// Can be a single object with `$` and `@lang`, or an array of them.
/// Falls through `.p.$` for abstract-shaped payloads.
pub(crate) fn extract_text_by_lang(val: &serde_json::Value) -> String {
    if val.is_null() {
        return String::new();
    }

    let items = if val.is_array() {
        val.as_array().unwrap().as_slice()
    } else {
        std::slice::from_ref(val)
    };

    for item in items {
        if item["@lang"].as_str() == Some("en")
            && let Some(text) = item_text(item)
        {
            return text.to_string();
        }
    }

    for item in items {
        if let Some(text) = item_text(item) {
            return text.to_string();
        }
    }

    val.as_str().unwrap_or("").to_string()
}

/// EPO wraps long-form text either directly in `$` (titles) or nested under
/// `p.$` (abstracts). Try direct first, then the paragraph wrapper.
fn item_text(item: &serde_json::Value) -> Option<&str> {
    item["$"].as_str().or_else(|| item["p"]["$"].as_str())
}

/// Extract date from document-id array (look for epodoc format).
pub(crate) fn extract_date(doc_ids: &serde_json::Value) -> Option<String> {
    if doc_ids.is_null() {
        return None;
    }

    let items = if doc_ids.is_array() {
        doc_ids.as_array().unwrap().as_slice()
    } else {
        std::slice::from_ref(doc_ids)
    };

    for item in items {
        if let Some(date) = item["date"]["$"].as_str() {
            // EPO dates are YYYYMMDD, convert to YYYY-MM-DD
            if date.len() == 8 {
                return Some(format!("{}-{}-{}", &date[..4], &date[4..6], &date[6..8]));
            }
            return Some(date.to_string());
        }
    }

    None
}

/// Extract full IPC classification codes (e.g. `B28B1/29`).
///
/// Reads two EPO blocks:
///   * `classification-ipc.text[].$` — already in compact form like `B28B1/29`.
///   * `classifications-ipcr.classification-ipcr[].text.$` — padded form like
///     `"B28B   1/    29            A I"`, normalised to `B28B1/29`.
///
/// De-duped, no truncation.
pub(crate) fn extract_classifications(biblio: &serde_json::Value) -> Vec<String> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();

    let ipc = &biblio["classification-ipc"]["text"];
    for item in iter_array_or_one(ipc) {
        if let Some(code) = item["$"].as_str() {
            let code = code.trim().to_string();
            if !code.is_empty() && seen.insert(code.clone()) {
                result.push(code);
            }
        }
    }

    let ipcr = &biblio["classifications-ipcr"]["classification-ipcr"];
    for item in iter_array_or_one(ipcr) {
        if let Some(text) = item["text"]["$"].as_str() {
            let code = normalize_ipcr_text(text);
            if !code.is_empty() && seen.insert(code.clone()) {
                result.push(code);
            }
        }
    }

    result
}

/// Extract CPC codes from the structured `patent-classifications` block.
///
/// Each entry has `section`/`class`/`subclass`/`main-group`/`subgroup`. We
/// concatenate to compact form (`B28B1/29`) so callers can compare IPC and CPC
/// using the same string shape.
pub(crate) fn extract_cpc_classifications(biblio: &serde_json::Value) -> Vec<String> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();

    let items = &biblio["patent-classifications"]["patent-classification"];
    for item in iter_array_or_one(items) {
        let scheme = item["classification-scheme"]["@scheme"]
            .as_str()
            .unwrap_or("");
        // CPCI = Inventive, CPCA = Additional. Skip non-CPC schemes (e.g. NEW, ECLA).
        if !matches!(scheme, "CPCI" | "CPCA" | "CPC") {
            continue;
        }
        let section = item["section"]["$"].as_str().unwrap_or("");
        let class = item["class"]["$"].as_str().unwrap_or("");
        let subclass = item["subclass"]["$"].as_str().unwrap_or("");
        let main = item["main-group"]["$"].as_str().unwrap_or("");
        let sub = item["subgroup"]["$"].as_str().unwrap_or("");
        if section.is_empty() || class.is_empty() || subclass.is_empty() {
            continue;
        }
        let code = format!("{section}{class}{subclass}{main}/{sub}");
        if seen.insert(code.clone()) {
            result.push(code);
        }
    }

    result
}

/// Extract inventors, preferring epodoc-format entries (which carry the
/// canonical name + country code suffix). Falls back to original-format if
/// no epodoc inventor is present.
pub(crate) fn extract_inventors(biblio: &serde_json::Value) -> Vec<String> {
    let items = &biblio["parties"]["inventors"]["inventor"];
    extract_party_names(items)
}

/// Extract all distinct applicants (assignees), preferring epodoc-format
/// entries; falls back to original-format when no epodoc entry exists.
pub(crate) fn extract_applicants_all(biblio: &serde_json::Value) -> Vec<String> {
    let items = &biblio["parties"]["applicants"]["applicant"];
    extract_party_names(items)
}

fn extract_party_names(items: &serde_json::Value) -> Vec<String> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();

    let entries: Vec<&serde_json::Value> = iter_array_or_one(items);
    let has_epodoc = entries
        .iter()
        .any(|e| e["@data-format"].as_str() == Some("epodoc"));

    for entry in &entries {
        let format = entry["@data-format"].as_str().unwrap_or("");
        // If any epodoc entries exist, take only those — original duplicates them
        // with formatting differences (mixed case, punctuation).
        if has_epodoc && format != "epodoc" {
            continue;
        }
        let name = entry["applicant-name"]["name"]["$"]
            .as_str()
            .or_else(|| entry["inventor-name"]["name"]["$"].as_str())
            .or_else(|| entry["applicant-name"]["$"].as_str())
            .or_else(|| entry["inventor-name"]["$"].as_str())
            .unwrap_or("")
            .trim();
        if !name.is_empty() && seen.insert(name.to_string()) {
            result.push(name.to_string());
        }
    }

    result
}

/// Earliest priority claim date (YYYY-MM-DD), if any. EPO returns priority
/// claims with epodoc `date.$` in YYYYMMDD form.
pub(crate) fn extract_priority_date(biblio: &serde_json::Value) -> Option<String> {
    let claims = &biblio["priority-claims"]["priority-claim"];
    let mut best: Option<String> = None;

    for claim in iter_array_or_one(claims) {
        let doc_ids = &claim["document-id"];
        for did in iter_array_or_one(doc_ids) {
            if did["@document-id-type"].as_str() != Some("epodoc") {
                continue;
            }
            let Some(date) = did["date"]["$"].as_str() else {
                continue;
            };
            if date.len() != 8 {
                continue;
            }
            let formatted = format!("{}-{}-{}", &date[..4], &date[4..6], &date[6..8]);
            best = Some(match best {
                Some(b) if b <= formatted => b,
                _ => formatted,
            });
        }
    }

    best
}

/// EPO IPCR text is space-padded: `"B28B   1/    29            A I"`. The
/// trailing single-letter tokens are flags (A = advanced, I = inventive). We
/// drop them and concatenate the code parts to compact form (`B28B1/29`).
fn normalize_ipcr_text(text: &str) -> String {
    let mut out = String::new();
    for tok in text.split_whitespace() {
        let is_flag = tok.len() == 1 && tok.chars().all(|c| c.is_ascii_alphabetic());
        if is_flag {
            break;
        }
        out.push_str(tok);
    }
    out
}

fn iter_array_or_one(val: &serde_json::Value) -> Vec<&serde_json::Value> {
    if let Some(arr) = val.as_array() {
        arr.iter().collect()
    } else if val.is_null() {
        Vec::new()
    } else {
        vec![val]
    }
}

/// Parse citations from EPO biblio response.
///
/// EPO's biblio endpoint can return multiple `exchange-document` entries (A1,
/// B1, …); citations sometimes live only on the application doc (A1) while
/// the granted doc (B1) carries no `references-cited` block. We walk every
/// document and merge with dedup so the caller doesn't depend on which kind
/// the chosen biblio happens to be.
///
/// Forward citations (who cites this patent) are not available from biblio —
/// that needs a separate search call. `citing` stays empty.
pub fn parse_citations(json: &serde_json::Value) -> Citations {
    let mut cited = Vec::new();
    let mut seen = HashSet::new();

    for doc in locate_exchange_docs(json) {
        let refs = &doc["bibliographic-data"]["references-cited"]["citation"];
        for cit in iter_array_or_one(refs) {
            let patcit = &cit["patcit"];
            if patcit.is_null() {
                continue;
            }

            let phase = cit["@cited-phase"].as_str().unwrap_or("").to_string();
            let category = cit["category"]["$"]
                .as_str()
                .map(str::to_string)
                .filter(|s| !s.is_empty());
            let cited_by = cit["@cited-by"]
                .as_str()
                .map(str::to_string)
                .filter(|s| !s.is_empty());

            let doc_ids = iter_array_or_one(&patcit["document-id"]);
            let date = extract_date(&patcit["document-id"]);
            let name = doc_ids
                .iter()
                .find_map(|d| d["name"]["$"].as_str())
                .map(str::to_string)
                .filter(|s| !s.is_empty());

            for did in &doc_ids {
                let doc_type = did["@document-id-type"].as_str().unwrap_or("");
                if doc_type != "epodoc" && doc_type != "docdb" {
                    continue;
                }
                let country = did["country"]["$"].as_str().unwrap_or("");
                let doc_number = did["doc-number"]["$"].as_str().unwrap_or("");
                if doc_number.is_empty() {
                    continue;
                }
                let patent_id = if doc_type == "epodoc" {
                    doc_number.to_string()
                } else {
                    format!("{country}{doc_number}")
                };
                if !seen.insert(patent_id.clone()) {
                    continue;
                }
                cited.push(Citation {
                    patent_id,
                    phase: phase.clone(),
                    category: category.clone(),
                    cited_by: cited_by.clone(),
                    date: date.clone(),
                    name: name.clone(),
                });
                break;
            }
        }
    }

    Citations {
        cited,
        citing: Vec::new(),
    }
}

/// Parse a full-text description response. EPO's shape:
///
/// ```text
/// ops:world-patent-data.ftxt:fulltext-documents.ftxt:fulltext-document.description
///   .@lang             // language tag
///   .p                 // single object or array of paragraph objects
///     .@num            // "0001", "0002", … (optional)
///     .$               // paragraph text
/// ```
///
/// Empty paragraphs are skipped. The convenience `plain_text` field joins
/// all paragraph bodies with `"\n\n"` for embedding / LLM contexts.
pub fn parse_description(json: &serde_json::Value, patent_id: &str) -> PatentDescription {
    let desc = &json["ops:world-patent-data"]["ftxt:fulltext-documents"]["ftxt:fulltext-document"]
        ["description"];

    if desc.is_null() {
        return PatentDescription {
            patent_id: patent_id.to_string(),
            ..Default::default()
        };
    }

    let language = desc["@lang"].as_str().map(str::to_string);

    let mut paragraphs = Vec::new();
    for item in iter_array_or_one(&desc["p"]) {
        let num = item["@num"].as_str().map(str::to_string);
        let text = item_text(item).unwrap_or("").trim().to_string();
        if !text.is_empty() {
            paragraphs.push(DescriptionParagraph { num, text });
        }
    }

    let plain_text = paragraphs
        .iter()
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");

    PatentDescription {
        patent_id: patent_id.to_string(),
        language,
        paragraphs,
        plain_text,
    }
}

/// Parse a claims response. EPO's shape:
///
/// ```text
/// ops:world-patent-data.ftxt:fulltext-documents.ftxt:fulltext-document.claims
///   .@lang             // language tag
///   .claim             // single object or array
///     .@id             // "claim001" (optional)
///     .@num            // "0001" (optional)
///     .claim-text      // string, object with `$`, or array
/// ```
///
/// `claim-text` can carry inline formatting (italics, subscripts, math
/// formulas) as nested elements; this parser flattens them to plain text.
/// If formatting matters for your downstream, walk the raw JSON yourself.
pub fn parse_claims(json: &serde_json::Value, patent_id: &str) -> PatentClaims {
    let claims_field = &json["ops:world-patent-data"]["ftxt:fulltext-documents"]["ftxt:fulltext-document"]
        ["claims"];

    if claims_field.is_null() {
        return PatentClaims {
            patent_id: patent_id.to_string(),
            ..Default::default()
        };
    }

    let language = claims_field["@lang"].as_str().map(str::to_string);

    let mut claims = Vec::new();
    for item in iter_array_or_one(&claims_field["claim"]) {
        let num = item["@num"].as_str().map(str::to_string);
        let id = item["@id"].as_str().map(str::to_string);
        let claim_text = &item["claim-text"];

        // EPO sometimes packs multiple numbered claims into a single
        // <claim> wrapper as an array of self-contained claim-text leaves
        // — distinct from the inline-formatting shape where the array
        // mixes strings with nested <sub>/<i>/<sup> tag objects. The
        // wrapper carrying its own @num/@id signals a single logical
        // claim with formatting; absence of both, plus a leaf-only array,
        // signals a packed claim set that must be split.
        if num.is_none()
            && id.is_none()
            && let Some(arr) = claim_text.as_array()
            && arr.len() > 1
            && arr.iter().all(is_leaf_claim_text)
        {
            for elem in arr {
                let text = flatten_claim_text(elem);
                if !text.is_empty() {
                    claims.push(Claim {
                        num: None,
                        id: None,
                        text,
                    });
                }
            }
        } else {
            let text = flatten_claim_text(claim_text);
            if !text.is_empty() {
                claims.push(Claim { num, id, text });
            }
        }
    }

    let plain_text = claims
        .iter()
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");

    PatentClaims {
        patent_id: patent_id.to_string(),
        language,
        claims,
        plain_text,
    }
}

/// Walk an EPO `claim-text` value and concatenate every text fragment in
/// document order, flattening nested inline formatting (`<i>`, `<sub>`,
/// `<sup>`, math, …) which EPO encodes as nested JSON objects with their
/// own `$` payloads.
///
/// Handles bare string / single object / array shapes and recurses into
/// any object/array children. `$` is the canonical XML-text key
/// (visited first to keep document order); `@`-prefixed keys are
/// attributes and skipped. Intra-fragment whitespace is preserved so
/// inline formatting boundaries don't collapse words together; the
/// outer string is trimmed once at the end.
///
/// Use this when you want plain text. If you need to preserve formatting,
/// walk the raw JSON yourself.
fn flatten_claim_text(val: &serde_json::Value) -> String {
    let mut out = String::new();
    collect_text_fragments(val, &mut out);
    out.trim().to_string()
}

/// `true` if `val` is a self-contained EPO claim-text leaf — a bare string
/// or an object whose only data key is `$` (the canonical XML-text key).
/// Used to distinguish a packed claim set (`claim-text` is an array of
/// such leaves, each a numbered claim) from an inline-formatting payload
/// (array mixes strings with `<sub>` / `<i>` / `<sup>` tag objects).
fn is_leaf_claim_text(val: &serde_json::Value) -> bool {
    match val {
        serde_json::Value::String(_) => true,
        serde_json::Value::Object(map) => map.keys().all(|k| k == "$" || k.starts_with('@')),
        _ => false,
    }
}

/// Recursive helper for [`flatten_claim_text`]. Appends every string
/// value reachable from `val` to `out` in document order. Preserves
/// internal whitespace; trimming is the caller's responsibility.
fn collect_text_fragments(val: &serde_json::Value, out: &mut String) {
    match val {
        serde_json::Value::String(s) => out.push_str(s),
        serde_json::Value::Array(arr) => {
            for item in arr {
                collect_text_fragments(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            if let Some(text) = map.get("$") {
                collect_text_fragments(text, out);
            }
            for (k, v) in map {
                if k == "$" || k.starts_with('@') {
                    continue;
                }
                collect_text_fragments(v, out);
            }
        }
        _ => {}
    }
}

/// Parse INPADOC family response to extract family members.
///
/// Returns one entry per publication. EPO emits multiple publications per
/// `patent_id` (e.g. EP1000000 appears as both A1 in 2000-05-17 and B1 in
/// 2003-02-12 — same patent at different stages); both are returned, in the
/// order EPO sent them. Callers that want only the granted version per
/// country can filter on `kind`.
pub fn parse_family(json: &serde_json::Value) -> Vec<FamilyMember> {
    let members =
        iter_array_or_one(&json["ops:world-patent-data"]["ops:patent-family"]["ops:family-member"]);

    let mut result: Vec<FamilyMember> = Vec::new();

    for member in members {
        let pub_ref = &member["publication-reference"]["document-id"];
        let doc_ids = iter_array_or_one(pub_ref);
        if doc_ids.is_empty() {
            continue;
        }

        // Pick the first epodoc/docdb id; that is the canonical publication ref.
        let mut chosen: Option<&serde_json::Value> = None;
        for doc_id in &doc_ids {
            let doc_type = doc_id["@document-id-type"].as_str().unwrap_or("");
            if doc_type == "epodoc" || doc_type == "docdb" {
                chosen = Some(doc_id);
                break;
            }
        }
        let Some(doc_id) = chosen else { continue };

        let doc_type = doc_id["@document-id-type"].as_str().unwrap_or("");
        let country = doc_id["country"]["$"].as_str().unwrap_or("").to_string();
        let doc_number = doc_id["doc-number"]["$"].as_str().unwrap_or("");
        let kind = doc_id["kind"]["$"].as_str().unwrap_or("").to_string();

        if doc_number.is_empty() {
            continue;
        }

        let patent_id = if doc_type == "epodoc" {
            doc_number.to_string()
        } else {
            format!("{country}{doc_number}")
        };

        result.push(FamilyMember {
            patent_id,
            country,
            kind,
            title: extract_text_by_lang(&member["invention-title"]),
            publication_date: extract_date(pub_ref),
        });
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_family_empty() {
        let json = serde_json::json!({});
        let result = parse_family(&json);
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_family_single_member() {
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "ops:patent-family": {
                    "ops:family-member": {
                        "publication-reference": {
                            "document-id": [
                                {
                                    "@document-id-type": "epodoc",
                                    "country": {"$": "EP"},
                                    "doc-number": {"$": "EP1234567"},
                                    "kind": {"$": "A1"}
                                }
                            ]
                        },
                        "invention-title": {"$": "Test invention", "@lang": "en"}
                    }
                }
            }
        });
        let result = parse_family(&json);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].patent_id, "EP1234567");
        assert_eq!(result[0].country, "EP");
        assert_eq!(result[0].kind, "A1");
        assert_eq!(result[0].title, "Test invention");
    }

    #[test]
    fn test_parse_family_multiple_members() {
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "ops:patent-family": {
                    "ops:family-member": [
                        {
                            "publication-reference": {
                                "document-id": [
                                    {
                                        "@document-id-type": "epodoc",
                                        "country": {"$": "EP"},
                                        "doc-number": {"$": "EP111"},
                                        "kind": {"$": "A1"}
                                    }
                                ]
                            },
                            "invention-title": {"$": "Invention A", "@lang": "en"}
                        },
                        {
                            "publication-reference": {
                                "document-id": [
                                    {
                                        "@document-id-type": "epodoc",
                                        "country": {"$": "US"},
                                        "doc-number": {"$": "US222"},
                                        "kind": {"$": "B1"}
                                    }
                                ]
                            },
                            "invention-title": {"$": "Invention B", "@lang": "en"}
                        }
                    ]
                }
            }
        });
        let result = parse_family(&json);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].patent_id, "EP111");
        assert_eq!(result[1].patent_id, "US222");
    }

    #[test]
    fn test_parse_family_keeps_all_publications_per_patent() {
        // Same patent at A1 (application) and B1 (granted) — both returned,
        // in the order EPO sent them.
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "ops:patent-family": {
                    "ops:family-member": [
                        {
                            "publication-reference": {
                                "document-id": [
                                    {
                                        "@document-id-type": "epodoc",
                                        "country": {"$": "EP"},
                                        "date": {"$": "20000517"},
                                        "doc-number": {"$": "EP111"},
                                        "kind": {"$": "A1"}
                                    }
                                ]
                            },
                            "invention-title": {"$": "Same patent", "@lang": "en"}
                        },
                        {
                            "publication-reference": {
                                "document-id": [
                                    {
                                        "@document-id-type": "epodoc",
                                        "country": {"$": "EP"},
                                        "date": {"$": "20030212"},
                                        "doc-number": {"$": "EP111"},
                                        "kind": {"$": "B1"}
                                    }
                                ]
                            },
                            "invention-title": {"$": "Same patent v2", "@lang": "en"}
                        }
                    ]
                }
            }
        });
        let result = parse_family(&json);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].kind, "A1");
        assert_eq!(result[0].publication_date.as_deref(), Some("2000-05-17"));
        assert_eq!(result[1].kind, "B1");
        assert_eq!(result[1].publication_date.as_deref(), Some("2003-02-12"));
    }

    #[test]
    fn test_parse_family_keeps_all_publications_when_no_dates() {
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "ops:patent-family": {
                    "ops:family-member": [
                        {
                            "publication-reference": {
                                "document-id": [{
                                    "@document-id-type": "epodoc",
                                    "country": {"$": "EP"},
                                    "doc-number": {"$": "EP111"},
                                    "kind": {"$": "A1"}
                                }]
                            }
                        },
                        {
                            "publication-reference": {
                                "document-id": [{
                                    "@document-id-type": "epodoc",
                                    "country": {"$": "EP"},
                                    "doc-number": {"$": "EP111"},
                                    "kind": {"$": "A2"}
                                }]
                            }
                        }
                    ]
                }
            }
        });
        let result = parse_family(&json);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].kind, "A1");
        assert_eq!(result[1].kind, "A2");
    }

    #[test]
    fn test_parse_search_results_empty() {
        let json = serde_json::json!({});
        let result = parse_search_results(&json);
        assert_eq!(result.total_count, 0);
        assert!(result.patents.is_empty());
    }

    #[test]
    fn test_parse_search_results_single() {
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "ops:biblio-search": {
                    "@total-result-count": "1",
                    "ops:search-result": {
                        "ops:range": {"@begin": "1", "@end": "1"},
                        "exchange-documents": {
                            "exchange-document": {
                                "bibliographic-data": {
                                    "invention-title": [{"$": "Test title", "@lang": "en"}],
                                    "parties": {
                                        "applicants": {
                                            "applicant": [{"applicant-name": {"name": {"$": "Acme Corp"}}}]
                                        }
                                    },
                                    "publication-reference": {
                                        "document-id": [{
                                            "@document-id-type": "epodoc",
                                            "doc-number": {"$": "EP1234567"}
                                        }]
                                    },
                                    "application-reference": {
                                        "document-id": [{"date": {"$": "20200115"}}]
                                    },
                                    "classifications-ipcr": {
                                        "classification-ipcr": [{"text": {"$": "H01M 10/48"}}]
                                    }
                                },
                                "abstract": [{"$": "Test abstract text", "@lang": "en"}]
                            }
                        }
                    }
                }
            }
        });
        let result = parse_search_results(&json);
        assert_eq!(result.total_count, 1);
        assert_eq!(result.range, (1, 1));
        assert_eq!(result.patents.len(), 1);
        assert_eq!(result.patents[0].patent_id, "EP1234567");
        assert_eq!(result.patents[0].title, "Test title");
        assert_eq!(result.patents[0].abstract_text, "Test abstract text");
        assert_eq!(result.patents[0].assignee.as_deref(), Some("Acme Corp"));
        assert_eq!(result.patents[0].filing_date.as_deref(), Some("2020-01-15"));
    }

    #[test]
    fn test_parse_search_results_multiple() {
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "ops:biblio-search": {
                    "@total-result-count": "42",
                    "ops:search-result": {
                        "ops:range": {"@begin": "1", "@end": "2"},
                        "exchange-documents": {
                            "exchange-document": [
                                {
                                    "bibliographic-data": {
                                        "invention-title": [{"$": "Patent A", "@lang": "en"}],
                                        "publication-reference": {
                                            "document-id": [{
                                                "@document-id-type": "epodoc",
                                                "doc-number": {"$": "EP111"}
                                            }]
                                        }
                                    },
                                    "abstract": [{"$": "Abstract A", "@lang": "en"}]
                                },
                                {
                                    "bibliographic-data": {
                                        "invention-title": [{"$": "Patent B", "@lang": "en"}],
                                        "publication-reference": {
                                            "document-id": [{
                                                "@document-id-type": "epodoc",
                                                "doc-number": {"$": "US222"}
                                            }]
                                        }
                                    },
                                    "abstract": [{"$": "Abstract B", "@lang": "en"}]
                                }
                            ]
                        }
                    }
                }
            }
        });
        let result = parse_search_results(&json);
        assert_eq!(result.total_count, 42);
        assert_eq!(result.range, (1, 2));
        assert_eq!(result.patents.len(), 2);
        assert_eq!(result.patents[0].patent_id, "EP111");
        assert_eq!(result.patents[1].patent_id, "US222");
    }

    #[test]
    fn test_parse_search_results_deduplicates() {
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "ops:biblio-search": {
                    "@total-result-count": "2",
                    "ops:search-result": {
                        "ops:range": {"@begin": "1", "@end": "2"},
                        "exchange-documents": {
                            "exchange-document": [
                                {
                                    "bibliographic-data": {
                                        "invention-title": [{"$": "Same", "@lang": "en"}],
                                        "publication-reference": {
                                            "document-id": [{"@document-id-type": "epodoc", "doc-number": {"$": "EP111"}}]
                                        }
                                    }
                                },
                                {
                                    "bibliographic-data": {
                                        "invention-title": [{"$": "Same v2", "@lang": "en"}],
                                        "publication-reference": {
                                            "document-id": [{"@document-id-type": "epodoc", "doc-number": {"$": "EP111"}}]
                                        }
                                    }
                                }
                            ]
                        }
                    }
                }
            }
        });
        let result = parse_search_results(&json);
        assert_eq!(
            result.patents.len(),
            1,
            "Duplicate patent IDs should be deduplicated"
        );
    }

    #[test]
    fn test_parse_biblio_empty() {
        let json = serde_json::json!({});
        let result = parse_biblio(&json);
        assert!(result.title.is_empty());
        assert!(result.abstract_text.is_empty());
        assert!(result.applicants.is_empty());
        assert!(result.inventors.is_empty());
        assert!(result.kind_code.is_none());
        assert!(result.family_id.is_none());
        assert!(result.cpc_classifications.is_empty());
    }

    /// Mirrors the real EP1000000 response shape: two exchange-documents
    /// (A1 + B1), abstract nested under `p.$`, full IPC codes via two blocks
    /// (`classification-ipc` + `classifications-ipcr`), CPC under
    /// `patent-classifications`, inventors + priority claim. Verifies the
    /// parser picks B1, extracts every field, and produces full IPC codes.
    #[test]
    fn test_parse_biblio_real_shape_a1_b1() {
        let abstract_obj = serde_json::json!({
            "@lang": "en",
            "p": {"$": "An apparatus for manufacturing green bricks."}
        });
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "exchange-documents": {
                    "exchange-document": [
                        {
                            "@country": "EP",
                            "@doc-number": "1000000",
                            "@family-id": "19768124",
                            "@kind": "A1",
                            "abstract": abstract_obj,
                            "bibliographic-data": {
                                "invention-title": [
                                    {"@lang": "de", "$": "DE title"},
                                    {"@lang": "en", "$": "Apparatus for manufacturing green bricks"}
                                ],
                                "publication-reference": {
                                    "document-id": [{
                                        "@document-id-type": "epodoc",
                                        "date": {"$": "20000517"},
                                        "doc-number": {"$": "EP1000000"}
                                    }]
                                },
                                "application-reference": {
                                    "document-id": [{
                                        "@document-id-type": "epodoc",
                                        "date": {"$": "19991108"},
                                        "doc-number": {"$": "EP19990203729"}
                                    }]
                                },
                                "references-cited": {
                                    "citation": [{
                                        "@cited-phase": "national-search-report",
                                        "patcit": {
                                            "document-id": [{
                                                "@document-id-type": "epodoc",
                                                "doc-number": {"$": "EP0680812"}
                                            }]
                                        }
                                    }]
                                }
                            }
                        },
                        {
                            "@country": "EP",
                            "@doc-number": "1000000",
                            "@family-id": "19768124",
                            "@kind": "B1",
                            "abstract": abstract_obj,
                            "bibliographic-data": {
                                "invention-title": [
                                    {"@lang": "en", "$": "Apparatus for manufacturing green bricks"}
                                ],
                                "publication-reference": {
                                    "document-id": [{
                                        "@document-id-type": "epodoc",
                                        "date": {"$": "20030212"},
                                        "doc-number": {"$": "EP1000000"}
                                    }]
                                },
                                "application-reference": {
                                    "document-id": [{
                                        "@document-id-type": "epodoc",
                                        "date": {"$": "19991108"},
                                        "doc-number": {"$": "EP19990203729"}
                                    }]
                                },
                                "classification-ipc": {
                                    "text": [
                                        {"$": "B28B1/29"},
                                        {"$": "B28B5/02"}
                                    ]
                                },
                                "classifications-ipcr": {
                                    "classification-ipcr": [
                                        {"@sequence": "1", "text": {"$": "B28B   1/    29            A I"}},
                                        {"@sequence": "2", "text": {"$": "H02P   6/    08            A I"}}
                                    ]
                                },
                                "patent-classifications": {
                                    "patent-classification": [{
                                        "@sequence": "1",
                                        "classification-scheme": {"@office": "EP", "@scheme": "CPCI"},
                                        "section": {"$": "B"},
                                        "class": {"$": "28"},
                                        "subclass": {"$": "B"},
                                        "main-group": {"$": "1"},
                                        "subgroup": {"$": "29"}
                                    }]
                                },
                                "priority-claims": {
                                    "priority-claim": {
                                        "document-id": [{
                                            "@document-id-type": "epodoc",
                                            "date": {"$": "19981112"},
                                            "doc-number": {"$": "NL19981010536"}
                                        }]
                                    }
                                },
                                "parties": {
                                    "applicants": {
                                        "applicant": [
                                            {
                                                "@data-format": "epodoc",
                                                "@sequence": "1",
                                                "applicant-name": {"name": {"$": "BOER BEHEER NIJMEGEN BV DE [NL]"}}
                                            },
                                            {
                                                "@data-format": "original",
                                                "@sequence": "1",
                                                "applicant-name": {"name": {"$": "BEHEERMAATSCHAPPIJ DE BOER NIJMEGEN B.V."}}
                                            }
                                        ]
                                    },
                                    "inventors": {
                                        "inventor": [{
                                            "@data-format": "epodoc",
                                            "inventor-name": {"name": {"$": "KOSMAN WILHELMUS JACOBUS MARIA [NL]"}}
                                        }]
                                    }
                                }
                            }
                        }
                    ]
                }
            }
        });

        let r = parse_biblio(&json);

        assert_eq!(r.title, "Apparatus for manufacturing green bricks");
        assert_eq!(
            r.abstract_text,
            "An apparatus for manufacturing green bricks."
        );
        assert_eq!(
            r.kind_code.as_deref(),
            Some("B1"),
            "should prefer B1 over A1"
        );
        assert_eq!(r.family_id.as_deref(), Some("19768124"));
        assert_eq!(
            r.publication_date.as_deref(),
            Some("2003-02-12"),
            "publication_date should come from chosen B1 doc, not A1"
        );
        assert_eq!(r.filing_date.as_deref(), Some("1999-11-08"));
        assert_eq!(r.priority_date.as_deref(), Some("1998-11-12"));
        assert_eq!(
            r.assignee.as_deref(),
            Some("BOER BEHEER NIJMEGEN BV DE [NL]")
        );
        assert_eq!(r.applicants, vec!["BOER BEHEER NIJMEGEN BV DE [NL]"]);
        assert_eq!(r.inventors, vec!["KOSMAN WILHELMUS JACOBUS MARIA [NL]"]);
        assert_eq!(
            r.classification,
            vec!["B28B1/29", "B28B5/02", "H02P6/08"],
            "should produce full IPC codes (compact form), de-duped across IPC + IPCR blocks"
        );
        assert_eq!(r.cpc_classifications, vec!["B28B1/29"]);
    }

    /// EP1000000's A1 has citations, B1 doesn't. Parser must merge across
    /// docs so the caller sees A1's citations even though we chose B1 for
    /// biblio.
    #[test]
    fn test_parse_citations_merges_across_a1_b1() {
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "exchange-documents": {
                    "exchange-document": [
                        {
                            "@kind": "A1",
                            "bibliographic-data": {
                                "references-cited": {
                                    "citation": {
                                        "@cited-phase": "search",
                                        "patcit": {
                                            "document-id": [{
                                                "@document-id-type": "epodoc",
                                                "doc-number": {"$": "EP0680812"}
                                            }]
                                        }
                                    }
                                }
                            }
                        },
                        {
                            "@kind": "B1",
                            "bibliographic-data": {}
                        }
                    ]
                }
            }
        });
        let r = parse_citations(&json);
        assert_eq!(r.cited.len(), 1);
        assert_eq!(r.cited[0].patent_id, "EP0680812");
    }

    #[test]
    fn test_normalize_ipcr_drops_flags() {
        // 2-digit subgroup, basic shape.
        assert_eq!(
            normalize_ipcr_text("B28B   1/    29            A I"),
            "B28B1/29"
        );
        // Different section, same width.
        assert_eq!(
            normalize_ipcr_text("H02P   6/    08            A I"),
            "H02P6/08"
        );
        // 4-digit subgroup (subdivided main-group).
        assert_eq!(
            normalize_ipcr_text("G05B  19/  4093            A I"),
            "G05B19/4093"
        );
        // 5-digit subgroup — common in CPC-aligned IPCR for new subdivisions.
        assert_eq!(
            normalize_ipcr_text("G05B  19/  40938            A I"),
            "G05B19/40938"
        );
        // 3-digit subgroup with leading zero significance.
        assert_eq!(
            normalize_ipcr_text("G06T   7/   246            A I"),
            "G06T7/246"
        );
        // Single-digit subgroup with `00` padding still normalises cleanly.
        assert_eq!(
            normalize_ipcr_text("B28B   7/    00            A I"),
            "B28B7/00"
        );
        // Non-inventive flag (just `A` instead of `A I`).
        assert_eq!(
            normalize_ipcr_text("A61B   3/   113            A"),
            "A61B3/113"
        );
        // No flags at all (terse form).
        assert_eq!(normalize_ipcr_text("B28B 1/29"), "B28B1/29");
        // Empty / whitespace-only input shouldn't panic.
        assert_eq!(normalize_ipcr_text(""), "");
        assert_eq!(normalize_ipcr_text("   "), "");
    }

    #[test]
    fn test_extract_text_by_lang_handles_p_wrapper() {
        // Abstract shape: { "@lang": "en", "p": { "$": "..." } }
        let val = serde_json::json!({"@lang": "en", "p": {"$": "Abstract body."}});
        assert_eq!(extract_text_by_lang(&val), "Abstract body.");

        // Title shape: { "@lang": "en", "$": "..." }
        let val = serde_json::json!({"@lang": "en", "$": "Title body."});
        assert_eq!(extract_text_by_lang(&val), "Title body.");
    }

    #[test]
    fn test_parse_citations_empty() {
        let json = serde_json::json!({});
        let result = parse_citations(&json);
        assert!(result.cited.is_empty());
        assert!(result.citing.is_empty());
    }

    #[test]
    fn test_parse_citations_single() {
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "exchange-documents": {
                    "exchange-document": {
                        "bibliographic-data": {
                            "references-cited": {
                                "citation": {
                                    "@cited-phase": "search",
                                    "patcit": {
                                        "document-id": [
                                            {
                                                "@document-id-type": "epodoc",
                                                "country": {"$": "US"},
                                                "doc-number": {"$": "US7654321"},
                                                "kind": {"$": "A1"}
                                            }
                                        ]
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
        let result = parse_citations(&json);
        assert_eq!(result.cited.len(), 1);
        assert_eq!(result.cited[0].patent_id, "US7654321");
        assert_eq!(result.cited[0].phase, "search");
    }

    #[test]
    fn test_parse_citations_multiple() {
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "exchange-documents": {
                    "exchange-document": {
                        "bibliographic-data": {
                            "references-cited": {
                                "citation": [
                                    {
                                        "@cited-phase": "search",
                                        "patcit": {
                                            "document-id": [
                                                {
                                                    "@document-id-type": "epodoc",
                                                    "country": {"$": "US"},
                                                    "doc-number": {"$": "US111"},
                                                    "kind": {"$": "A1"}
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "@cited-phase": "examination",
                                        "patcit": {
                                            "document-id": [
                                                {
                                                    "@document-id-type": "epodoc",
                                                    "country": {"$": "EP"},
                                                    "doc-number": {"$": "EP222"},
                                                    "kind": {"$": "B1"}
                                                }
                                            ]
                                        }
                                    }
                                ]
                            }
                        }
                    }
                }
            }
        });
        let result = parse_citations(&json);
        assert_eq!(result.cited.len(), 2);
        assert_eq!(result.cited[0].patent_id, "US111");
        assert_eq!(result.cited[1].patent_id, "EP222");
    }

    #[test]
    fn test_parse_citations_deduplicates() {
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "exchange-documents": {
                    "exchange-document": {
                        "bibliographic-data": {
                            "references-cited": {
                                "citation": [
                                    {
                                        "@cited-phase": "search",
                                        "patcit": {
                                            "document-id": [{
                                                "@document-id-type": "epodoc",
                                                "country": {"$": "US"},
                                                "doc-number": {"$": "US111"},
                                                "kind": {"$": "A1"}
                                            }]
                                        }
                                    },
                                    {
                                        "@cited-phase": "examination",
                                        "patcit": {
                                            "document-id": [{
                                                "@document-id-type": "epodoc",
                                                "country": {"$": "US"},
                                                "doc-number": {"$": "US111"},
                                                "kind": {"$": "A2"}
                                            }]
                                        }
                                    }
                                ]
                            }
                        }
                    }
                }
            }
        });
        let result = parse_citations(&json);
        assert_eq!(
            result.cited.len(),
            1,
            "Duplicate patent IDs should be deduplicated"
        );
    }

    #[test]
    fn test_parse_throttling_idle() {
        let s = parse_throttling_header(
            "idle (images=green:200, inpadoc=green:60, retrieval=green:200, search=green:30)",
            None,
            None,
        )
        .unwrap();
        assert_eq!(s.load, ThrottlingLoad::Idle);
        assert_eq!(s.endpoints["search"].remaining_per_minute, 30);
        assert_eq!(s.endpoints["search"].color, ThrottlingColor::Green);
        assert!(!s.is_exhausted());
    }

    #[test]
    fn test_parse_throttling_busy_real_shape() {
        // Verbatim from a real EPO biblio response.
        let header = "busy (images=green:100, inpadoc=green:45, other=green:1000, retrieval=green:100, search=green:15)";
        let s = parse_throttling_header(header, Some(20360), Some(1427448)).unwrap();
        assert_eq!(s.load, ThrottlingLoad::Busy);
        assert_eq!(s.inpadoc_remaining(), Some(45));
        assert_eq!(s.search_remaining(), Some(15));
        assert_eq!(s.retrieval_remaining(), Some(100));
        assert_eq!(s.hour_bytes_used, Some(20360));
        assert_eq!(s.week_bytes_used, Some(1427448));
    }

    #[test]
    fn test_parse_throttling_color_progression() {
        let s = parse_throttling_header(
            "overloaded (search=yellow:5, retrieval=red:1, images=black:0)",
            None,
            None,
        )
        .unwrap();
        assert_eq!(s.load, ThrottlingLoad::Overloaded);
        assert_eq!(s.endpoints["search"].color, ThrottlingColor::Yellow);
        assert_eq!(s.endpoints["retrieval"].color, ThrottlingColor::Red);
        assert_eq!(s.endpoints["images"].color, ThrottlingColor::Black);
        assert!(s.is_exhausted(), "any black endpoint => exhausted");
    }

    #[test]
    fn test_parse_throttling_zero_remaining_treated_as_exhausted() {
        // remaining=0 even with green color is effectively exhausted.
        let s = parse_throttling_header("idle (search=green:0)", None, None).unwrap();
        assert!(s.is_exhausted());
    }

    #[test]
    fn test_parse_throttling_unknown_load_preserves_endpoints() {
        let s = parse_throttling_header("weird_state (inpadoc=green:45)", None, None).unwrap();
        assert_eq!(
            s.load,
            ThrottlingLoad::Other("weird_state".to_string()),
            "unknown load string should be preserved verbatim"
        );
        assert_eq!(s.inpadoc_remaining(), Some(45));
    }

    #[test]
    fn test_parse_throttling_malformed_entries_skipped() {
        // The good entry survives; the malformed one is dropped silently.
        let s = parse_throttling_header(
            "idle (inpadoc=green:45, malformed-entry, search=:notanumber)",
            None,
            None,
        )
        .unwrap();
        assert_eq!(s.endpoints.len(), 1);
        assert_eq!(s.inpadoc_remaining(), Some(45));
    }

    #[test]
    fn test_parse_description_empty_returns_patent_id() {
        let r = parse_description(&serde_json::json!({}), "EP1");
        assert_eq!(r.patent_id, "EP1");
        assert!(r.language.is_none());
        assert!(r.paragraphs.is_empty());
        assert!(r.plain_text.is_empty());
    }

    #[test]
    fn test_parse_description_single_paragraph() {
        // EPO sometimes returns `p` as a single object, not an array.
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "ftxt:fulltext-documents": {
                    "ftxt:fulltext-document": {
                        "description": {
                            "@lang": "en",
                            "p": {"@num": "0001", "$": "  The invention.  "}
                        }
                    }
                }
            }
        });
        let r = parse_description(&json, "EP1");
        assert_eq!(r.language.as_deref(), Some("en"));
        assert_eq!(r.paragraphs.len(), 1);
        assert_eq!(r.paragraphs[0].num.as_deref(), Some("0001"));
        assert_eq!(r.paragraphs[0].text, "The invention.");
        assert_eq!(r.plain_text, "The invention.");
    }

    #[test]
    fn test_parse_description_multi_paragraph_joins_blank_skipped() {
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "ftxt:fulltext-documents": {
                    "ftxt:fulltext-document": {
                        "description": {
                            "@lang": "en",
                            "p": [
                                {"@num": "0001", "$": "BACKGROUND OF THE INVENTION"},
                                {"@num": "0002", "$": ""},      // empty: dropped
                                {"@num": "0003", "$": "Para three."},
                                {"$": "Untagged."}              // no @num
                            ]
                        }
                    }
                }
            }
        });
        let r = parse_description(&json, "EP1000000");
        assert_eq!(r.paragraphs.len(), 3, "empty paragraph should be skipped");
        assert_eq!(r.paragraphs[0].text, "BACKGROUND OF THE INVENTION");
        assert_eq!(r.paragraphs[1].num.as_deref(), Some("0003"));
        assert_eq!(r.paragraphs[2].num, None);
        assert_eq!(
            r.plain_text,
            "BACKGROUND OF THE INVENTION\n\nPara three.\n\nUntagged."
        );
    }

    #[test]
    fn test_parse_claims_empty_returns_patent_id() {
        let r = parse_claims(&serde_json::json!({}), "EP1");
        assert_eq!(r.patent_id, "EP1");
        assert!(r.language.is_none());
        assert!(r.claims.is_empty());
        assert!(r.plain_text.is_empty());
    }

    #[test]
    fn test_parse_claims_single_claim() {
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "ftxt:fulltext-documents": {
                    "ftxt:fulltext-document": {
                        "claims": {
                            "@lang": "en",
                            "claim": {
                                "@id": "claim001",
                                "@num": "0001",
                                "claim-text": {"$": "1. A widget comprising a sprocket."}
                            }
                        }
                    }
                }
            }
        });
        let r = parse_claims(&json, "EP1");
        assert_eq!(r.language.as_deref(), Some("en"));
        assert_eq!(r.claims.len(), 1);
        assert_eq!(r.claims[0].id.as_deref(), Some("claim001"));
        assert_eq!(r.claims[0].num.as_deref(), Some("0001"));
        assert_eq!(r.claims[0].text, "1. A widget comprising a sprocket.");
    }

    #[test]
    fn test_parse_claims_flattens_inline_formatting() {
        // EPO encodes inline tags (italics, subscripts, superscripts, math)
        // as nested objects whose text lives under their own `$`. The
        // flatten must recurse and preserve intra-fragment whitespace.
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "ftxt:fulltext-documents": {
                    "ftxt:fulltext-document": {
                        "claims": {
                            "@lang": "en",
                            "claim": {
                                "@num": "0001",
                                "claim-text": [
                                    "1. A widget with mass m",
                                    {"sub": {"$": "0"}},
                                    " comprising a lattice of ",
                                    {"i": {"$": "polycrystalline"}},
                                    " silicon doped at 10",
                                    {"sup": {"$": "15"}},
                                    " atoms/cm",
                                    {"sup": {"$": "3"}},
                                    "."
                                ]
                            }
                        }
                    }
                }
            }
        });
        let r = parse_claims(&json, "EP1");
        assert_eq!(r.claims.len(), 1);
        // Subscripts/superscripts/italics flattened in document order;
        // intra-fragment whitespace preserved.
        assert_eq!(
            r.claims[0].text,
            "1. A widget with mass m0 comprising a lattice of polycrystalline silicon doped at 1015 atoms/cm3."
        );
    }

    #[test]
    fn test_parse_claims_packed_claim_set_in_single_wrapper() {
        // Real EP1000000 shape: one <claim> wrapper with no @num, holding
        // an array of 11 self-contained <claim-text> leaves — each a
        // numbered claim. Must split into separate Claim entries instead
        // of concatenating into one mega-claim.
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "ftxt:fulltext-documents": {
                    "ftxt:fulltext-document": {
                        "claims": {
                            "@lang": "EN",
                            "claim": {
                                "claim-text": [
                                    {"$": "1. A widget."},
                                    {"$": "2. The widget of claim 1."},
                                    {"$": "3. The widget of claim 2."}
                                ]
                            }
                        }
                    }
                }
            }
        });
        let r = parse_claims(&json, "EP1");
        assert_eq!(r.claims.len(), 3, "packed claim set must split per leaf");
        assert_eq!(r.claims[0].text, "1. A widget.");
        assert_eq!(r.claims[1].text, "2. The widget of claim 1.");
        assert_eq!(r.claims[2].text, "3. The widget of claim 2.");
        // Inline formatting case must still be handled as one claim:
        // covered by test_parse_claims_flattens_inline_formatting.
    }

    #[test]
    fn test_parse_claims_multi_with_array_text_and_formatting() {
        // claim-text as array with nested object (simulating inline formatting).
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "ftxt:fulltext-documents": {
                    "ftxt:fulltext-document": {
                        "claims": {
                            "@lang": "en",
                            "claim": [
                                {
                                    "@num": "0001",
                                    "claim-text": [
                                        "1. A method comprising:",
                                        {"$": "applying force to a "},
                                        {"$": "widget"},
                                        "."
                                    ]
                                },
                                {
                                    "@num": "0002",
                                    "claim-text": "2. The method of claim 1, wherein the force is gradient."
                                },
                                {
                                    "@num": "0003",
                                    "claim-text": ""  // empty: dropped
                                }
                            ]
                        }
                    }
                }
            }
        });
        let r = parse_claims(&json, "EP1");
        assert_eq!(r.claims.len(), 2, "empty claim should be skipped");
        assert!(r.claims[0].text.contains("widget"));
        assert!(r.claims[0].text.contains("method"));
        assert_eq!(r.claims[1].num.as_deref(), Some("0002"));
        assert!(r.plain_text.contains("\n\n"));
    }

    #[test]
    fn test_parse_citations_extracts_metadata() {
        // Real EPO shape: each citation carries @cited-by, @cited-phase,
        // category.$, document-id[].name.$, document-id[epodoc].date.$.
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "exchange-documents": {
                    "exchange-document": {
                        "bibliographic-data": {
                            "references-cited": {
                                "citation": {
                                    "@cited-by": "examiner",
                                    "@cited-phase": "national-search-report",
                                    "category": {"$": "X"},
                                    "patcit": {
                                        "document-id": [
                                            {
                                                "@document-id-type": "epodoc",
                                                "date": {"$": "19951108"},
                                                "doc-number": {"$": "EP0680812"},
                                                "name": {"$": "BOER BEHEER NIJMEGEN BV DE"}
                                            },
                                            {
                                                "@document-id-type": "docdb",
                                                "country": {"$": "EP"},
                                                "doc-number": {"$": "0680812"},
                                                "kind": {"$": "A1"}
                                            }
                                        ]
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
        let r = parse_citations(&json);
        assert_eq!(r.cited.len(), 1);
        let c = &r.cited[0];
        assert_eq!(c.patent_id, "EP0680812");
        assert_eq!(c.phase, "national-search-report");
        assert_eq!(c.category.as_deref(), Some("X"));
        assert_eq!(c.cited_by.as_deref(), Some("examiner"));
        assert_eq!(c.date.as_deref(), Some("1995-11-08"));
        assert_eq!(c.name.as_deref(), Some("BOER BEHEER NIJMEGEN BV DE"));
    }

    #[test]
    fn test_parse_citations_skips_non_patent() {
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "exchange-documents": {
                    "exchange-document": {
                        "bibliographic-data": {
                            "references-cited": {
                                "citation": [
                                    {
                                        "nplcit": {
                                            "text": {"$": "Non-patent literature reference"}
                                        }
                                    },
                                    {
                                        "@cited-phase": "search",
                                        "patcit": {
                                            "document-id": [{
                                                "@document-id-type": "epodoc",
                                                "country": {"$": "US"},
                                                "doc-number": {"$": "US999"},
                                                "kind": {"$": "A1"}
                                            }]
                                        }
                                    }
                                ]
                            }
                        }
                    }
                }
            }
        });
        let result = parse_citations(&json);
        assert_eq!(result.cited.len(), 1);
        assert_eq!(result.cited[0].patent_id, "US999");
    }

    // --- parse_search_results: malformed-doc + dedup counting ---

    fn search_doc(country: &str, num: &str, kind: &str, title: &str) -> serde_json::Value {
        serde_json::json!({
            "bibliographic-data": {
                "publication-reference": {
                    "document-id": [{
                        "@document-id-type": "epodoc",
                        "country": {"$": country},
                        "doc-number": {"$": num},
                        "kind": {"$": kind}
                    }]
                },
                "invention-title": {"$": title, "@lang": "en"}
            }
        })
    }

    /// Mirrors the real EPO shape: `ops:range` lives on `ops:biblio-search`
    /// (NOT on `ops:search-result`), and `exchange-documents` is an array
    /// of `{exchange-document: {…}}` wrappers, one per result. Pre-fix the
    /// parser silently returned `range: (0,0), patents: []` for live data.
    #[test]
    fn test_parse_search_results_real_shape() {
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "ops:biblio-search": {
                    "@total-result-count": "194",
                    "ops:range": {"@begin": "1", "@end": "10"},
                    "ops:search-result": {
                        "exchange-documents": [
                            {
                                "exchange-document": {
                                    "@country": "CN",
                                    "@kind": "A",
                                    "@family-id": "89371517",
                                    "abstract": [
                                        {"@lang": "en", "p": {"$": "First abstract."}},
                                        {"@lang": "ol", "p": {"$": "其他语言"}}
                                    ],
                                    "bibliographic-data": {
                                        "invention-title": [
                                            {"@lang": "ol", "$": "其他"},
                                            {"@lang": "en", "$": "Hit one"}
                                        ],
                                        "publication-reference": {
                                            "document-id": [{
                                                "@document-id-type": "epodoc",
                                                "date": {"$": "20240105"},
                                                "doc-number": {"$": "CN111"}
                                            }]
                                        },
                                        "application-reference": {
                                            "document-id": [{
                                                "@document-id-type": "epodoc",
                                                "date": {"$": "20220526"},
                                                "doc-number": {"$": "CN20228037675"}
                                            }]
                                        },
                                        "classifications-ipcr": {
                                            "classification-ipcr": {
                                                "@sequence": "1",
                                                "text": {"$": "G01C  21/    34            A I"}
                                            }
                                        },
                                        "patent-classifications": {
                                            "patent-classification": [{
                                                "@sequence": "1",
                                                "classification-scheme": {"@scheme": "CPCI"},
                                                "section": {"$": "G"},
                                                "class": {"$": "01"},
                                                "subclass": {"$": "C"},
                                                "main-group": {"$": "21"},
                                                "subgroup": {"$": "34"}
                                            }]
                                        },
                                        "priority-claims": {
                                            "priority-claim": {
                                                "document-id": {
                                                    "@document-id-type": "epodoc",
                                                    "date": {"$": "20210702"},
                                                    "doc-number": {"$": "US202163218215P"}
                                                }
                                            }
                                        },
                                        "parties": {
                                            "applicants": {
                                                "applicant": [
                                                    {
                                                        "@data-format": "epodoc",
                                                        "@sequence": "1",
                                                        "applicant-name": {"name": {"$": "APPLE INC"}}
                                                    },
                                                    {
                                                        "@data-format": "original",
                                                        "@sequence": "1",
                                                        "applicant-name": {"name": {"$": "苹果公司"}}
                                                    }
                                                ]
                                            },
                                            "inventors": {
                                                "inventor": [{
                                                    "@data-format": "epodoc",
                                                    "@sequence": "1",
                                                    "inventor-name": {"name": {"$": "KIM YUN-JAE"}}
                                                }]
                                            }
                                        }
                                    }
                                }
                            },
                            {
                                "exchange-document": {
                                    "@country": "US",
                                    "@kind": "A1",
                                    "bibliographic-data": {
                                        "invention-title": {"@lang": "en", "$": "Hit two"},
                                        "publication-reference": {
                                            "document-id": [{
                                                "@document-id-type": "epodoc",
                                                "doc-number": {"$": "US222"}
                                            }]
                                        }
                                    }
                                }
                            }
                        ]
                    }
                }
            }
        });
        let r = parse_search_results(&json);
        assert_eq!(r.total_count, 194);
        assert_eq!(
            r.range,
            (1, 10),
            "range should come from biblio-search.ops:range"
        );
        assert_eq!(
            r.patents.len(),
            2,
            "should unwrap each `exchange-document` from the array"
        );

        let p0 = &r.patents[0];
        assert_eq!(p0.patent_id, "CN111");
        assert_eq!(p0.title, "Hit one");
        assert_eq!(p0.abstract_text, "First abstract.");
        assert_eq!(p0.kind_code.as_deref(), Some("A"));
        assert_eq!(p0.family_id.as_deref(), Some("89371517"));
        assert_eq!(p0.publication_date.as_deref(), Some("2024-01-05"));
        assert_eq!(p0.filing_date.as_deref(), Some("2022-05-26"));
        assert_eq!(p0.priority_date.as_deref(), Some("2021-07-02"));
        assert_eq!(p0.assignee.as_deref(), Some("APPLE INC"));
        assert_eq!(p0.applicants, vec!["APPLE INC"]);
        assert_eq!(p0.inventors, vec!["KIM YUN-JAE"]);
        assert_eq!(p0.classification, vec!["G01C21/34"]);
        assert_eq!(p0.cpc_classifications, vec!["G01C21/34"]);

        let p1 = &r.patents[1];
        assert_eq!(p1.patent_id, "US222");
        assert_eq!(p1.title, "Hit two");
        // p1 is the minimal/sparse variant — fields gracefully fall back to defaults.
        assert!(p1.applicants.is_empty());
        assert!(p1.inventors.is_empty());
        assert!(p1.priority_date.is_none());
    }

    #[test]
    fn test_parse_search_results_skips_missing_id() {
        // Doc with no publication-reference must be dropped (malformed) so
        // an empty-id patent row never surfaces to the tool caller.
        let json = serde_json::json!({
            "ops:world-patent-data": {
                "ops:biblio-search": {
                    "@total-result-count": "2",
                    "ops:search-result": {
                        "ops:range": {"@begin": "1", "@end": "2"},
                        "exchange-documents": {
                            "exchange-document": [
                                search_doc("EP", "EP42", "A1", "Good"),
                                serde_json::json!({
                                    "bibliographic-data": {
                                        "invention-title": {"$": "Orphan", "@lang": "en"}
                                    }
                                }),
                            ]
                        }
                    }
                }
            }
        });
        let result = parse_search_results(&json);
        assert_eq!(result.patents.len(), 1);
        assert_eq!(result.patents[0].patent_id, "EP42");
    }
}
