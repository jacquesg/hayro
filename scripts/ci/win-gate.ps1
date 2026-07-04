# The native windows-msvc test gate, run by .woodpecker/engine-windows-amd64.yaml on the
# beeld-ci-base windows image. Runs the nextest suite natively for
# x86_64-pc-windows-msvc — the windows half of the old tests-cross-platform job.
# The host-invariant lanes (fmt/clippy/features/nostd/doc/deny) run once on Linux
# (engine-linux-amd64.yaml). The windows image has no bash, so the gate is PowerShell.
$ErrorActionPreference = 'Stop'

# Windows MAX_PATH (260): let git + cargo's libgit2 use the \\?\ extended-length
# prefix so deeply-nested test-asset paths (assets/svgs) check out.
& git config --global core.longpaths true

# The svgraster dep is a private git.bloudraad.io repo. Rewrite its ssh:// URL to
# token-authenticated HTTPS with the read-only forge clone token (step env) so cargo
# (git-fetch-with-cli, .cargo/config.toml) fetches it with no SSH key on the runner.
# Models sdk/.woodpecker/ci-prelude.sh. Build the config key as ONE argument —
# PowerShell would mis-split the bash-style url."...".insteadOf form.
$forgeKey = "url.https://oauth2:$($env:FORGE_CLONE_TOKEN)@git.bloudraad.io/.insteadOf"
& git config --global $forgeKey "ssh://git@git.bloudraad.io/"

# Activate the baked mise toolchain for the workspace config — without `mise
# install`, `mise exec -- cargo` reports "cargo is not currently active" (the tools
# are baked into the image, so this is fast). GITHUB_TOKEN (step env) authenticates
# the version checks.
& mise trust --all
if ($LASTEXITCODE -ne 0) { throw 'mise trust failed' }
& mise install
if ($LASTEXITCODE -ne 0) { throw 'mise install failed' }

# sccache cannot cache incremental; mandatory as on the unix gates.
$env:CARGO_INCREMENTAL = '0'

# Redirect cargo target + sccache onto the agent's persistent C:/cache (no-op
# without it). Per-workflow CARGO_TARGET_DIR (the windows workspace volume denies
# target writes, os error 5). NOT CARGO_HOME: mise's rust is a rustup install whose
# cargo proxy lives under CARGO_HOME/bin, so redirecting it hides cargo.
if (Test-Path C:/cache) {
    $wf = if ($env:CI_WORKFLOW_NAME) { $env:CI_WORKFLOW_NAME } else { 'local' }
    $env:CARGO_TARGET_DIR = "C:/cache/target/beeld/$wf"
    $env:SCCACHE_DIR = 'C:/cache/sccache/beeld'
    if (Get-Command sccache -ErrorAction SilentlyContinue) { $env:RUSTC_WRAPPER = 'sccache' }
    New-Item -ItemType Directory -Force -Path $env:CARGO_TARGET_DIR | Out-Null
}

# beeld-tests' visual-regression suite drives external PDF viewers (absent on the
# CI image); run only its load:: subset here and every other crate's tests in
# full — mirroring `just ci-test` inline, since `just` needs sh (absent on the
# windows image).
Write-Host '+ cargo nextest run --locked --workspace (beeld-tests: load:: only)'
& mise exec -- cargo nextest run --locked --workspace --exclude beeld-demo --exclude beeld-fuzz --exclude beeld-jbig2 --exclude beeld-jpeg2000 -E 'not (package(beeld-tests) and not test(/load::/)) and not (package(beeld-syntax) and test(/pdf_version/))'
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
