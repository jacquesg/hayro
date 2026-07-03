# syntax=docker/dockerfile:1
#
# beeld CI base image, git.bloudraad.io/bloudraad/beeld-ci-base
#
# TOOLING ONLY: the mise toolchain plus the one system library the workspace
# links against. It bakes NO compiled dependency cache — third-party deps AND
# in-tree crates are cached at RUNTIME on the agent's persistent /cache volume
# (sccache + a per-repo CARGO_HOME and a per-workflow CARGO_TARGET_DIR, see
# .woodpecker/engine.yaml). That keeps the image small and rebuilt only when the
# toolchain (mise.toml) or the system libraries change, never on a dependency bump.
#
# The real build+push is the privileged docker-buildx step in
# .woodpecker/base-image.yaml.

ARG MISE_IMAGE=ghcr.io/jdx/mise:2026.5.18
FROM ${MISE_IMAGE} AS final

# The gates point cargo + sccache at the /cache volume per-repo/per-workflow, so
# nothing cargo writes lands in the image. RUST_MIN_STACK: rustc's codegen
# threads default to an 8 MiB stack, which deeply-nested generics can overflow,
# taking rustc down with SIGSEGV instead of a clean error; 32 MiB is what rustc's
# own overflow diagnostic recommends.
#
# RUSTUP_TOOLCHAIN: the mise image ships a system rustup that defaults to 1.96.0,
# whose LLVM 22.1 SIGSEGVs compiling regex at opt-level=3 — exactly what mise's
# cargo: backend does when it source-builds cargo-nextest/cargo-deny. This forces
# every rustup-proxied cargo/rustc onto the org baseline 1.96.1 (mise installs it;
# rustup auto-installs it otherwise), so those tool compiles use the fixed rustc,
# not the image's 1.96.0. Keep in sync with mise.toml.
ENV PATH=/mise/shims:$PATH \
    RUST_MIN_STACK=33554432 \
    RUSTUP_TOOLCHAIN=1.96.1

# System libraries the workspace links against under a full --workspace build.
#   pkg-config + libfontconfig1-dev : yeslogic-fontconfig-sys, pulled by fontdb
#       (beeld-svg / the tests), the one system font lib beeld links.
#   build-essential : a C toolchain (cc) for any dependency build script.
# No OpenSSL/FreeType/HarfBuzz: beeld is pure-Rust otherwise (flate2 uses the
# zlib-rs backend, text shaping is rustybuzz/skrifa, images are pure-Rust codecs).
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git \
        pkg-config libfontconfig1-dev \
        build-essential \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# mise.toml is the EXCLUSIVE toolchain source of truth. `mise install` provisions
# everything it declares: rust 1.95.0 + clippy/rustfmt, just, cargo-nextest (a
# PREBUILT binary via cargo.binstall) and sccache.
COPY mise.toml ./
# GITHUB_TOKEN (a BuildKit secret passed by the build-push step) authenticates
# the mise/aqua/binstall GitHub-release fetches to lift the unauthenticated
# (~60/hr) rate limit. Optional: absent on PR dry-runs, where mise falls back to
# unauthenticated (the `|| true` keeps the build working).
RUN --mount=type=secret,id=GITHUB_TOKEN \
    export GITHUB_TOKEN="$(cat /run/secrets/GITHUB_TOKEN 2>/dev/null || true)" && \
    mise trust --all && mise install
