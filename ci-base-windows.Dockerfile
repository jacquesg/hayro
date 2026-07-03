# escape=`
#
# beeld-ci-base (windows variant): the windows/amd64 toolchain image the
# engine-windows gate runs on. servercore + MSVC (VS Build Tools, for rust-msvc) +
# MinGit (cargo git fetches) + mise (the toolchain from the SAME mise.toml as the
# linux variant). beeld is pure-Rust with no C/crypto/bindings, so unlike sdk this
# needs no NASM, mingw-w64, or runtime toolchains. Built + pushed to a SEPARATE
# tag (beeld-ci-base-windows) by .woodpecker/base-image-windows.yaml via
# scripts/ci/base-image-windows-build.ps1 — no multi-arch stitch.
#
# escape=` makes backtick the line-escape so C:\ paths stay literal.
FROM mcr.microsoft.com/windows/servercore:10.0.26100.7462

SHELL ["powershell", "-NoProfile", "-Command", "$ErrorActionPreference='Stop'; $ProgressPreference='SilentlyContinue';"]

# VS Build Tools: MSVC (cl.exe / link.exe / Windows SDK / CRT). rust-msvc auto-detects
# it (vswhere); --includeRecommended lands the VC++ redist so the msvc-built mise/just/
# cargo binaries find VCRUNTIME140.dll (servercore ships none). Exit 3010 is
# success-with-reboot in a container.
RUN Invoke-WebRequest -UseBasicParsing 'https://aka.ms/vs/17/release/vs_buildtools.exe' -OutFile C:/vs_buildtools.exe ; $p = Start-Process -Wait -PassThru -FilePath C:/vs_buildtools.exe -ArgumentList '--quiet','--wait','--norestart','--nocache','--installPath','C:\BuildTools','--add','Microsoft.VisualStudio.Workload.VCTools','--includeRecommended' ; if ($p.ExitCode -ne 0 -and $p.ExitCode -ne 3010) { throw ('vs_buildtools exit ' + $p.ExitCode) } ; Remove-Item C:/vs_buildtools.exe

# MinGit: git for cargo's git-dependency fetches (system tool, not mise-managed; the
# linux ci-base likewise gets git from apt).
ARG GIT_VERSION=2.49.0
RUN curl.exe -L --fail --retry 5 --retry-all-errors --connect-timeout 30 -o C:/mingit.zip "https://github.com/git-for-windows/git/releases/download/v$($env:GIT_VERSION).windows.1/MinGit-$($env:GIT_VERSION)-64-bit.zip" ; if ($LASTEXITCODE -ne 0) { throw 'mingit download failed' } ; Expand-Archive C:/mingit.zip -DestinationPath C:/mingit -Force ; Remove-Item C:/mingit.zip

# mise: the toolchain SSOT. The ZIP ships mise-shim.exe (proper exe shims).
ARG MISE_VERSION=v2026.5.18
ENV MISE_DATA_DIR=C:/mise/data
ENV MISE_CACHE_DIR=C:/mise/cache
ENV MISE_GLOBAL_CONFIG_FILE=C:/mise/config.toml
RUN New-Item -ItemType Directory -Force -Path C:/mise/bin | Out-Null ; curl.exe -L --fail --retry 5 --retry-all-errors --connect-timeout 30 -o C:/mise.zip "https://github.com/jdx/mise/releases/download/$($env:MISE_VERSION)/mise-$($env:MISE_VERSION)-windows-x64.zip" ; if ($LASTEXITCODE -ne 0) { throw 'mise download failed' } ; Expand-Archive C:/mise.zip -DestinationPath C:/misetmp -Force ; Move-Item C:/misetmp/mise/bin/mise.exe C:/mise/bin/mise.exe ; Move-Item C:/misetmp/mise/bin/mise-shim.exe C:/mise/bin/mise-shim.exe ; Remove-Item -Recurse -Force C:/mise.zip, C:/misetmp ; & C:/mise/bin/mise.exe --version

# PATH: mise shims + mise bin + system tools (rebuilt from the Machine PATH; a bare
# ';%PATH%' would expand empty under the docker ENV and lose system32).
RUN [Environment]::SetEnvironmentVariable('PATH', ([Environment]::GetEnvironmentVariable('PATH','Machine') + ';C:\mise\data\shims;C:\mise\bin;C:\mingit\cmd'), 'Machine')

# Provision the toolchain from the SAME mise.toml the linux variant reads. All of
# beeld's mise tools (rust, just, cargo-nextest, cargo-deny, sccache) work on
# windows, so this is a blanket `mise install` — no MISE_DISABLE_TOOLS needed.
COPY mise.toml C:/mise/config.toml
# GITHUB_TOKEN (build-arg, from the github_token CI secret) authenticates mise's
# GitHub API calls during install (the unauthenticated 60 req/hr limit 403s). ARG,
# so it is build-time only, not baked into the image.
ARG GITHUB_TOKEN
RUN & C:/mise/bin/mise.exe trust --all ; & C:/mise/bin/mise.exe install ; & C:/mise/bin/mise.exe reshim ; & C:/mise/bin/mise.exe exec -- cargo --version ; & C:/mise/bin/mise.exe exec -- just --version
