# epo-client — async client for the EPO Open Patent Services (OPS) v3.2
# REST API. Run `just` (no args) to list every recipe.

[private]
default:
    @just --list

# Run unit + mock-HTTP + doc tests. Skips ignored live-API tests.
test:
    cargo test

# Live-API smoke tests (needs EPO_CONSUMER_KEY + EPO_CONSUMER_SECRET).
test-live:
    cargo test --test real_epo -- --ignored --test-threads=1 --nocapture

# Pre-PR gate: fmt + clippy + tests.
check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test

# Fuzz a target for `seconds` (default 60). e.g. `just fuzz fuzz_cql_validate 300`. Needs nightly.
fuzz target seconds="60":
    cargo +nightly fuzz run {{target}} -- -max_total_time={{seconds}}

# Dry-run publish — verify build + tarball, no upload.
publish-dry:
    cargo publish --dry-run

# Publish to crates.io. Name + version are immutable once accepted.
publish:
    cargo publish
