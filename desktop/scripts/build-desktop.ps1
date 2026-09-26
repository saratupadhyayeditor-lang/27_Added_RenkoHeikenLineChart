# Build the dedicated desktop platform on Windows:
# algo-server.exe + algo-desktop.exe + static UI.
# Also produces the updater release asset + SHA256SUMS in dist\.
$ErrorActionPreference = "Stop"

$Root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$Dist = Join-Path $Root "dist"

Write-Host "[1/5] building algo-server (release)"
cargo build --release -p algo-server --manifest-path (Join-Path $Root "Cargo.toml")

Write-Host "[2/5] building algo-desktop (release)"
cargo build --release --manifest-path (Join-Path $Root "desktop\Cargo.toml")

Write-Host "[3/5] assembling $Dist"
New-Item -ItemType Directory -Force -Path (Join-Path $Dist "static") | Out-Null
Copy-Item (Join-Path $Root "target\release\algo-server.exe") $Dist -Force
Copy-Item (Join-Path $Root "crates\server\static\*") (Join-Path $Dist "static") -Recurse -Force
Copy-Item (Join-Path $Root "desktop\target\release\algo-desktop.exe") $Dist -Force

$ArchRaw = $env:PROCESSOR_ARCHITECTURE
switch ($ArchRaw) {
  "AMD64" { $Arch = "x86_64" }
  "ARM64" { $Arch = "aarch64" }
  default { $Arch = $ArchRaw.ToLower() }
}
$Asset = "algo-desktop-windows-$Arch.zip"

Write-Host "[4/5] packaging update asset $Asset"
Remove-Item (Join-Path $Dist $Asset) -ErrorAction SilentlyContinue
Compress-Archive -Path @(
  (Join-Path $Dist "algo-server.exe"),
  (Join-Path $Dist "algo-desktop.exe"),
  (Join-Path $Dist "static")
) -DestinationPath (Join-Path $Dist $Asset) -Force
$Hash = (Get-FileHash (Join-Path $Dist $Asset) -Algorithm SHA256).Hash.ToLower()
"$Hash  $Asset" | Set-Content -Encoding ascii (Join-Path $Dist "SHA256SUMS")

Write-Host "[5/5] building single-file portable launcher"
$Launcher = "algo-desktop-windows-$Arch-portable.exe"
& (Join-Path $Root "desktop\target\release\algo-desktop.exe") --make-launcher `
  (Join-Path $Root "desktop\target\release\algo-desktop.exe") `
  (Join-Path $Root "target\release\algo-server.exe") `
  (Join-Path $Root "crates\server\static") `
  (Join-Path $Dist $Launcher)

Write-Host "done -> $Dist (folder + $Asset + SHA256SUMS + $Launcher)"
