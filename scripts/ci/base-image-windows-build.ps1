# Build + push the WINDOWS arch of beeld-ci-base to the per-arch PIN tags
# :latest-windows + :<sha>-windows. Driven by .woodpecker/base-image-windows-amd64.yaml;
# base-image-stitch.yaml assembles these pins with the linux-amd64/linux-arm64 pins
# into the multi-arch beeld-ci-base:latest.
#
# Windows-ci-base bootstrap: it builds the image every windows step runs on, so it
# cannot run on that image. It runs on bare servercore + downloads a docker CLI, and
# talks to the agent daemon over the npipe. Logic lives here (read verbatim): the
# windows woodpecker backend base64|iex-es inline step commands and its logging
# wrapper breaks on any quote.
#
# Requires env: REG_USER, REG_TOKEN, CI_COMMIT_SHA, GITHUB_TOKEN.

$ErrorActionPreference = 'Stop'
$repo   = 'git.bloudraad.io/bloudraad/beeld-ci-base'
$sha    = $env:CI_COMMIT_SHA
$latest = "${repo}:latest-windows"
$shaTag = "${repo}:${sha}-windows"
$cli    = 'C:/dockercli/docker/docker.exe'

Write-Host '=== fetch docker CLI (bare servercore has none; classic build + push, no buildx) ==='
Invoke-WebRequest 'https://download.docker.com/win/static/stable/x86_64/docker-29.5.2.zip' -OutFile C:/docker.zip
Expand-Archive -Path C:/docker.zip -DestinationPath C:/dockercli -Force
& $cli --version

Write-Host '=== build windows arch (servercore + MSVC + MinGit + mise) ==='
# GITHUB_TOKEN (the github_token CI secret) authenticates mise's GitHub API calls
# during install, lifting the unauthenticated 60 req/hr limit.
& $cli build --build-arg GITHUB_TOKEN=$env:GITHUB_TOKEN -f ci-base-windows.Dockerfile -t $latest -t $shaTag .
if ($LASTEXITCODE -ne 0) { throw "docker build failed ($LASTEXITCODE)" }

Write-Host '=== login + push ==='
& $cli login git.bloudraad.io -u $env:REG_USER -p $env:REG_TOKEN
if ($LASTEXITCODE -ne 0) { throw "docker login failed ($LASTEXITCODE)" }
& $cli push $latest
if ($LASTEXITCODE -ne 0) { throw "push $latest failed ($LASTEXITCODE)" }
& $cli push $shaTag
if ($LASTEXITCODE -ne 0) { throw "push $shaTag failed ($LASTEXITCODE)" }

Write-Host "=== done: pushed $latest + $shaTag ==="
