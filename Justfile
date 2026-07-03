# beeld task runner (mise-provisioned). Every .woodpecker/*.yaml step is
# `just <recipe>`, and each recipe self-wraps its tools in `mise exec`, so the
# exact same commands run locally and in CI — local == CI. See
# .woodpecker/README.md.

# Apply rustfmt to the whole workspace.
fmt:
    mise exec -- cargo fmt --all

# -----------------------------------------------------------------------------
# CI gates — the single source of truth for what Woodpecker runs.
# -----------------------------------------------------------------------------

# The whole host-invariant engine gate in one pass: rustfmt + clippy +
# per-feature check + no_std + nextest + rustdoc + cargo-deny, sharing one warm
# CARGO_TARGET_DIR so each check reuses the previous one's compilation. Runs
# every check and AGGREGATES failures (not fail-fast), so a single run reports
# every problem. This is the body of .woodpecker/engine-linux-amd64.yaml.
ci-engine:
    #!/usr/bin/env bash
    set -uo pipefail
    # Enable sccache only when it's actually on PATH (the base image bakes it via
    # mise.toml). Guarded + exported so the nested cargo inherits it, and so a
    # local `just ci-engine` still works without sccache installed.
    if command -v sccache >/dev/null 2>&1; then
      export RUSTC_WRAPPER=sccache
    fi
    rc=0
    just ci-fmt      || rc=1
    just ci-clippy   || rc=1
    just ci-features || rc=1
    just ci-nostd    || rc=1
    just ci-test     || rc=1
    just ci-doc      || rc=1
    just ci-deny     || rc=1
    # Surface the sccache hit/miss counts; without this the gate passes (just
    # slower) when /cache is not a persistent mount, so every lookup silently
    # misses. Non-blocking, never changes rc.
    if command -v sccache >/dev/null 2>&1; then
      sccache --show-stats || true
    fi
    exit "$rc"

# rustfmt check. Stable channel — beeld has no nightly-gated rustfmt options.
ci-fmt:
    mise exec -- cargo fmt --all -- --check

# Clippy, full workspace, warnings denied. `-D warnings` rides the clippy
# invocation, NOT RUSTFLAGS — a global RUSTFLAGS changes every dependency's
# fingerprint and busts the sccache/target cache. beeld-demo (wasm cdylib) and
# beeld-bench (criterion harness) are excluded, as in the old GitHub gate.
ci-clippy:
    mise exec -- cargo clippy --locked --workspace --exclude beeld-demo --exclude beeld-bench --tests --examples -- -D warnings

# Per-feature check — each feature in isolation (cargo-hack --each-feature),
# catching features that fail to compile alone.
ci-features:
    mise exec -- cargo hack check --locked --each-feature --workspace --exclude beeld-demo --exclude beeld-bench

# no_std compatibility — the embedded-facing crates must build for a bare-metal
# target without std (ports the old GitHub no_std job). Self-provisions the target
# (idempotent) so the gate is host-independent and doesn't rely on mise's `targets`.
ci-nostd:
    mise exec -- rustup target add thumbv6m-none-eabi
    mise exec -- cargo check --locked -p beeld-ccitt --target thumbv6m-none-eabi
    mise exec -- cargo check --locked -p beeld-jbig2 --no-default-features --target thumbv6m-none-eabi
    mise exec -- cargo check --locked -p beeld-jpeg2000 --no-default-features --target thumbv6m-none-eabi
    mise exec -- cargo check --locked -p beeld-syntax --no-default-features --target thumbv6m-none-eabi

# Test suite via nextest. Tests needing corpora fetched by `sync.py` (not in the
# repo) are excluded so the gate is offline/hermetic: beeld-jbig2/beeld-jpeg2000
# (their `asset_suite` errors even to list without inputs), beeld-tests' visual/
# render/write suites (drive external viewers via sitro — only its `load::` subset
# runs, matching the old GitHub gate), and beeld-syntax's `pdf_version_*` tests
# (read beeld-tests/downloads/*.pdf). Every other crate's unit tests run in full;
# demo (wasm) and fuzz excluded too.
ci-test:
    mise exec -- cargo nextest run --locked --workspace --all-features --exclude beeld-demo --exclude beeld-fuzz --exclude beeld-jbig2 --exclude beeld-jpeg2000 -E 'not (package(beeld-tests) and not test(/load::/)) and not (package(beeld-syntax) and test(/pdf_version/))'

# rustdoc with warnings denied (a broken intra-doc link fails the gate).
ci-doc:
    RUSTDOCFLAGS="-D warnings" mise exec -- cargo doc --locked --workspace --exclude beeld-demo --exclude beeld-bench --no-deps

# cargo-deny supply-chain policy (advisories, licences, bans, sources) against
# the committed Cargo.lock (deny.toml).
ci-deny:
    mise exec -- cargo deny --locked check
