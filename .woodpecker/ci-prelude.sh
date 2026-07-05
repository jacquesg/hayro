#!/usr/bin/env bash
# Shared per-step prelude for the .woodpecker engine gates. Woodpecker steps share
# the workspace but not $HOME, so every step that compiles must re-apply the forge
# auth and toolchain:
#   - rewrite the ssh:// private deps (svgraster / henog) to authenticated HTTPS
#     with the read-only clone token (FORGE_CLONE_TOKEN, from the step secret),
#     so no SSH key or host-key trust is needed on the runner;
#   - ensure the mise-pinned toolchain is present.
#
# Must be run AFTER `. scripts/ci-cache-env.sh`, which on the darwin local backend
# restores $HOME to the passwd home -- the `git config --global` below must land in
# the same home cargo's git subprocess reads, or the rewrite is invisible and the
# fetch hangs on ssh.
set -euo pipefail
# Clear a stale global-config lock a killed step may have left: the darwin local backend
# shares one $HOME across workflows, and an interrupted `git config --global` leaves
# ~/.gitconfig.lock, which then fails every later run ("could not lock config file ...:
# File exists"). The engine workflows are serialised, so no live writer holds it; on the
# ephemeral docker containers the file never exists (a no-op).
rm -f "${HOME}/.gitconfig.lock" 2>/dev/null || true
git config --global url."https://oauth2:${FORGE_CLONE_TOKEN}@git.bloudraad.io/".insteadOf "ssh://git@git.bloudraad.io/"
mise trust --all
mise install
# Repair a half-written toolchain a killed `mise install` may have left. This darwin local
# backend shares ONE ~/.local/share/mise across every repo's gate, and mise provisions rust
# via rustup: installs/rust/<v>/bin/cargo is a symlink to the bundled rustup binary. An
# interrupted reinstall can leave that rustup (and the proxy) gone while mise still records
# rust as "installed" -- so the `mise install` above no-ops and every later `mise exec --
# cargo` dies with "couldn't exec process: No such file or directory". `mise install --force`
# cannot recover it: --force uninstalls first, and mise shells that out to the now-missing
# `rustup toolchain uninstall` (os error 2). So when cargo will not exec, delete the install
# dir directly (bypassing that) and reinstall from scratch, which re-bootstraps rustup. The
# guard execs cargo in milliseconds on a healthy toolchain -- a no-op.
if ! mise exec -- cargo --version >/dev/null 2>&1; then
  rust_dir="$(mise where rust 2>/dev/null || true)"
  case "$rust_dir" in */installs/rust/*) rm -rf "$rust_dir" ;; esac
  mise install rust
fi
