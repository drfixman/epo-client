# epo-client

[![crates.io](https://img.shields.io/crates/v/epo-client.svg)](https://crates.io/crates/epo-client)
[![docs.rs](https://docs.rs/epo-client/badge.svg)](https://docs.rs/epo-client)

Async Rust client for the [EPO Open Patent Services](https://ops.epo.org)
v3.2 REST API. OAuth2 with token caching, biblio / family / citations /
search / description / claims, tolerant parsing of EPO's nested JSON
shapes, per-endpoint pacing, and a CQL syntax validator.

```toml
[dependencies]
epo-client = "0.1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

The package is `epo-client`; the lib is `epo`:

```rust
use epo::{EpoClient, EpoError};

#[tokio::main]
async fn main() -> Result<(), EpoError> {
    let client = EpoClient::new(
        std::env::var("EPO_CONSUMER_KEY").unwrap(),
        std::env::var("EPO_CONSUMER_SECRET").unwrap(),
        None,
    );
    let biblio = client.fetch_biblio("EP1000000").await?;
    println!("{}: {}", biblio.title, biblio.assignee.as_deref().unwrap_or(""));
    Ok(())
}
```

See [docs.rs](https://docs.rs/epo-client) for the full API. Get a free
consumer key + secret at <https://developers.epo.org>.

## License

MIT — see [LICENSE](LICENSE).
