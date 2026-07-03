# beeld CI (Woodpecker)

Ported from the old GitHub Actions workflow (`.github/workflows/ci.yml`) to the
org's Woodpecker + `mise` + `just` convention (as in `moegoe` / `sdk`).

`mise.toml` is the toolchain source of truth; the `Justfile` is the single
source of truth for what runs, so **local == CI** — every gate is `just
<recipe>` and each recipe wraps its tools in `mise exec`. Run any gate locally
with `just ci-engine` (or an individual `just ci-clippy`, `just ci-test`, …).

## What runs today

The base image `beeld-ci-base:latest` is **multi-arch**: three per-arch pins are
built natively and stitched into one manifest list, so every engine gate pulls the
same tag and the daemon resolves its arch.

| Workflow | Platform | Purpose |
|---|---|---|
| `base-image-linux-amd64.yaml` | linux/amd64 | Builds the amd64 pin `:latest-linux-amd64` (`ci-base.Dockerfile`: mise toolchain + `libfontconfig1-dev`). |
| `base-image-linux-arm64.yaml` | linux/arm64 | Builds the arm64 pin `:latest-linux-arm64` (same Dockerfile, built natively — QEMU cross-build is too slow). |
| `base-image-windows-amd64.yaml` | windows/amd64 | Builds the windows pin `:latest-windows` (servercore + MSVC + MinGit + mise) via `scripts/ci/base-image-windows-build.ps1`. |
| `base-image-stitch.yaml` | linux/amd64 | `depends_on` the three builds; `docker buildx imagetools create` stitches the pins into `beeld-ci-base:latest` + `:<sha>`. |
| `engine-linux-amd64.yaml` | linux/amd64 | The host-invariant gate in one pass: `just ci-engine` = rustfmt · clippy · per-feature check (cargo-hack) · no_std · nextest · rustdoc · **cargo-deny**. |
| `engine-linux-arm64.yaml` | linux/arm64 | `just ci-test` (nextest) natively. |
| `engine-darwin-arm64.yaml` | darwin/arm64 | `just ci-test` on the `local` backend (macOS has no containers); hand-authenticated clone + persistent checkout. |
| `engine-windows-amd64.yaml` | windows/amd64 | nextest natively (beeld-tests `load::` subset, every other crate in full) via `scripts/ci/win-gate.ps1` (PowerShell — the image has no bash). |

Base-image rebuilds are path-filtered to a toolchain (`mise.toml`) or
image-definition change — never a dependency bump. The host-invariant lanes run
once on amd64; arm64/darwin/Windows run only the tests natively. This ports the
whole old workflow (`checks` / `min-version` / `no_std` / `load_tests`) and adds
native arm64/darwin/windows test lanes; the beeld-tests visual-regression suite
stays out of CI (see below).

## Operator prerequisites (one-off)

1. **Trusted repo** — mark `bloudraad/beeld` *Trusted* in Woodpecker (required
   for the `/cache` volume mount and the privileged buildx step).
2. **Registry** — a Forgejo robot/token with package read+write on
   `bloudraad/beeld`; add Woodpecker repo secrets `forge_registry_user` and
   `forge_registry_token` (do **not** expose them to `pull_request` events).
3. **`github_token`** — a repo secret so `mise`/binstall clear the shared
   unauthenticated GitHub rate limit.
4. **Agents** — the gates span four platforms: `linux/amd64`, `linux/arm64`,
   `darwin/arm64` (a `local`-backend Mac), and `windows/amd64` (docker, windows
   containers). Each needs a registered agent online.
5. **`/cache` volume** — a persistent volume on each linux/windows agent; the
   engine gates keep `CARGO_TARGET_DIR` + sccache (and cargo's caches) there.
   Without it the gate still passes, just cold-compiles every run.
6. **Bootstrap the base image FIRST.** The engine gates pull
   `beeld-ci-base:latest`, so it must exist **and be multi-arch** before they
   run. Trigger the base image **manually** once (or push a base-image change):
   `base-image-linux-amd64`, `base-image-linux-arm64`, and `base-image-windows`
   build their per-arch pins, then `base-image-stitch` (`depends_on` all three)
   assembles `:latest`. Only after the stitch does each arch's engine gate resolve
   its variant. The first engine run per agent cold-compiles the
   vello/resvg/usvg graph — raise that run's timeout.

## Not yet ported (next increments)

- **Visual regression.** beeld-tests' heavy suite renders PDFs and diffs them
  against reference images across external viewers (ghostscript, mupdf, poppler,
  pdfbox, pdfium) via `sitro`, which needs those viewers plus Docker inside a
  Trusted agent. Only the `load::` subset runs in the engine gates today; the
  full visual suite is best baked into a dedicated image.
