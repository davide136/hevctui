# Compila hevctui, crea il setup con Inno Setup e pubblica la release su GitHub.
# La versione è quella di Cargo.toml: alzala prima di lanciare lo script.
#   .\scripts\release.ps1             build + setup + push + release vX.Y.Z
#   .\scripts\release.ps1 -NoPublish  solo build + setup in dist\
param([switch]$NoPublish)
# 'Continue': in PowerShell 5.1 lo stderr di cargo/gh diventa ErrorRecord; gli errori veri si leggono da $LASTEXITCODE
$ErrorActionPreference = 'Continue'
Set-Location (Split-Path $PSScriptRoot -Parent)

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
$tag = "v$version"

if (-not $NoPublish) {
    if (git status --porcelain) { throw "Working tree non pulito: committa prima di pubblicare." }
    $tags = gh release list --json tagName --jq '.[].tagName'
    if ($tags -contains $tag) { throw "La release $tag esiste già: alza la versione in Cargo.toml." }
}

# toolchain GNU: as/dlltool di mingw-w64 (WinLibs da winget) se non sono già nel PATH
if (-not (Get-Command dlltool -ErrorAction SilentlyContinue)) {
    $mingw = Get-Item "$env:LOCALAPPDATA\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.*\mingw64\bin" -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($mingw) { $env:Path = "$($mingw.FullName);$env:Path" }
}
cargo build --release
if ($LASTEXITCODE) { throw "cargo build fallita" }

$targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path (Get-Location) 'target' }
$exe = Join-Path $targetDir 'release\hevctui.exe'

$iscc = @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe") |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { $iscc = (Get-Command iscc -ErrorAction Stop).Source }
& $iscc /Qp "/DAppVersion=$version" "/DSourceExe=$exe" installer\hevctui.iss
if ($LASTEXITCODE) { throw "compilazione setup fallita" }
$setup = "dist\hevctui-setup-$version.exe"
Write-Host "Setup: $setup"
if ($NoPublish) { return }

git push origin HEAD
if ($LASTEXITCODE) { throw "git push fallito" }
gh release create $tag $setup --title "hevctui $version" --generate-notes --target (git rev-parse HEAD)
if ($LASTEXITCODE) { throw "gh release create fallito" }
