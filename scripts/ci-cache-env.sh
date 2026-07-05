# Sourced by the .woodpecker steps before any cargo/just (never executed directly).
# It points cargo + sccache at the agent's PERSISTENT cache so deps + compiled crates
# survive across runs: the /cache volume on the docker agents (linux/windows), or
# ~/woodpecker-cache on the darwin LOCAL backend (macOS SIP blocks /cache at root and
# the local backend mounts no volume, so ~/woodpecker-cache is staged outside the
# per-pipeline workspace the backend wipes). With neither (a dev machine, or an agent
# without one) it is a no-op -- `just ci-*` behaves identically, just cold.
#
# CARGO_HOME + SCCACHE_DIR are per-repo: cargo and sccache are concurrency-safe,
# so the git-dependency clones and the compiled-crate cache are shared across
# every workflow and persist across runs. CARGO_TARGET_DIR is per-WORKFLOW
# because concurrent workflows lock the target dir (a shared one would serialise
# them); within a workflow the serial gates reuse it. RUSTC_WRAPPER is set only
# when sccache is actually available, so a base image predating the sccache bake
# degrades to a cold compile instead of failing every rustc.
#
# CARGO_INCREMENTAL=0 is MANDATORY (matching moegoe/mangwhap): sccache cannot cache
# incremental compilations, and incremental artifacts are not portable across runs
# on the persistent target dir -- reusing them corrupts the cache (rust-lld then
# fails with "unknown relocation" on a stale/half-written object). CI builds are
# one-shot, so incremental buys nothing here anyway. Set unconditionally; this
# script is only ever sourced in CI, so local dev keeps incremental.
export CARGO_INCREMENTAL=0

# A larger rustc stack: deep const-eval in some dependency graphs overflows the base
# image's 32 MiB default (rustc SIGSEGV, "increase rustc's stack size by setting
# RUST_MIN_STACK=..."). 64 MiB is safe headroom.
export RUST_MIN_STACK=67108864

# Pick the persistent cache root: /cache (docker agents mount it) or ~/woodpecker-cache
# (darwin local backend). Empty -> no redirect (cold, but identical).
cache_root=""
if [ -d /cache ]; then
  cache_root=/cache
elif [ "$(uname -s)" = "Darwin" ]; then
  # The woodpecker local backend points HOME at a per-pipeline temp dir, but rustup
  # refuses to run when $HOME differs from the euid's passwd home ("you may be using
  # sudo"), and both the persistent cache and the agent's mise toolchain (incl sccache)
  # live under the real home. Restore it from the passwd db (~user expands via getpwnam,
  # not $HOME); eval re-parses so the tilde is unquoted at expansion time.
  HOME="$(eval echo ~"$(id -un)")"; export HOME
  # The local backend's gate PATH lacks the mise-managed tools -- unlike the linux docker
  # image, only `mise exec` brings them, so a recipe's bare invocation (and mise's own
  # tool resolution after the HOME restore) fails. Prepend the installed tools' bin dirs
  # (they persist under ~/.local/share/mise across runs) so cargo + the rest resolve, as
  # on linux. mise exec stays correct too; this just also covers the bare calls.
  PATH="$(mise bin-paths 2>/dev/null | tr '\n' ':')$PATH"; export PATH
  cache_root="$HOME/woodpecker-cache"
  # A bigger sccache budget for the native macOS build.
  export SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE:-32G}"
fi

if [ -n "$cache_root" ]; then
  export CARGO_HOME="$cache_root/cargo/${CI_REPO_NAME:-beeld}"
  export CARGO_TARGET_DIR="$cache_root/target/${CI_REPO_NAME:-beeld}/${CI_WORKFLOW_NAME:-local}"
  export SCCACHE_DIR="$cache_root/sccache/${CI_REPO_NAME:-beeld}"
  # The runbook stages the cache root, but create the per-repo subdirs in case it has
  # not (idempotent; cargo/sccache would create them anyway).
  mkdir -p "$CARGO_HOME" "$CARGO_TARGET_DIR" "$SCCACHE_DIR" 2>/dev/null || true
  # sccache may be a bare-PATH binary (docker base image) or mise-managed (darwin, where
  # it is only on the `mise exec` PATH, not the login PATH). Detect either; the gates run
  # cargo via `mise exec`, so RUSTC_WRAPPER=sccache resolves there.
  if command -v sccache >/dev/null 2>&1 || mise which sccache >/dev/null 2>&1; then
    export RUSTC_WRAPPER=sccache
  fi
fi
