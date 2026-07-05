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
# via rustup: installs/rust/<v>/bin/cargo is a symlink to the bundled rustup proxy. An
# interrupted reinstall can leave that proxy (or bin/rustup) gone while mise still records
# rust as "installed" -- so the `mise install` above no-ops and never repairs it, and every
# subsequent `mise exec -- cargo` dies with "couldn't exec process: No such file or
# directory". Execing cargo costs milliseconds on a healthy toolchain (a no-op); when it
# fails, force a clean rust reinstall so the gate self-heals instead of failing cryptically.
mise exec -- cargo --version >/dev/null 2>&1 || mise install --force rust
