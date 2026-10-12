# install.ps1 — install a published godot-mcp release on Windows (x86_64).
#
#   irm https://raw.githubusercontent.com/sunerpy/godot-mcp/main/scripts/install.ps1 | iex
#
# It downloads the Windows archive and SHA256SUMS.txt from the GitHub release,
# refuses an archive whose digest is not the published one, extracts the whole
# package, runs the package's own `self-check`, and adds the package's bin
# directory to the user PATH. The package stays together because the server
# finds its supervisor and the project assets it installs beside its own
# executable.
#
# Environment (prefixed GODOTMCP_, not GODOT_MCP_: the server treats every
# GODOT_MCP_ variable as its own configuration and refuses one it does not know):
#   GODOTMCP_VERSION        release to install, for example v0.2.0 (default: the latest)
#   GODOTMCP_INSTALL_ROOT   where packages are extracted (default: %LOCALAPPDATA%\godot-mcp)
#   GODOTMCP_DOWNLOAD_BASE  base URL of release downloads, for a mirror
#                            (default: https://github.com/sunerpy/godot-mcp/releases/download)
#   GODOTMCP_NO_PATH        set to 1 to leave the user PATH unchanged

$ErrorActionPreference = "Stop"

$Repo = "sunerpy/godot-mcp"
$ChecksumFile = "SHA256SUMS.txt"

function Die($Message) {
  Write-Error "godot-mcp install: $Message"
  exit 1
}

switch ($env:PROCESSOR_ARCHITECTURE) {
  "AMD64" { $Target = "x86_64-pc-windows-msvc" }
  "ARM64" { Die "no Windows ARM64 archive is published; build from source (README, Build from source)" }
  default { Die "unsupported architecture: $env:PROCESSOR_ARCHITECTURE" }
}

if ($env:GODOTMCP_VERSION) {
  $Version = $env:GODOTMCP_VERSION -replace '^v', ''
} else {
  try {
    $Release = Invoke-RestMethod `
      -Uri "https://api.github.com/repos/$Repo/releases/latest" `
      -Headers @{ "User-Agent" = "godot-mcp-install" }
  } catch {
    Die "found no published release (there is none before the first one); build from source (README, Build from source) or set GODOTMCP_VERSION"
  }
  $Version = $Release.tag_name -replace '^v', ''
}
if ($Version -notmatch '^[0-9A-Za-z.+-]+$') { Die "not a release version: $Version" }

$Package = "godot-mcp-$Version-$Target"
$Asset = "$Package.zip"
$DownloadBase = if ($env:GODOTMCP_DOWNLOAD_BASE) { $env:GODOTMCP_DOWNLOAD_BASE } else { "https://github.com/$Repo/releases/download" }
$BaseUrl = "$DownloadBase/v$Version"
$InstallRoot = if ($env:GODOTMCP_INSTALL_ROOT) { $env:GODOTMCP_INSTALL_ROOT } else { Join-Path $env:LOCALAPPDATA "godot-mcp" }
$TempDir = New-Item -ItemType Directory -Path (Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid()))

try {
  $Archive = Join-Path $TempDir $Asset
  $Checksums = Join-Path $TempDir $ChecksumFile
  Write-Host "Downloading $Asset"
  Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/$Asset" -OutFile $Archive
  Invoke-WebRequest -UseBasicParsing -Uri "$BaseUrl/$ChecksumFile" -OutFile $Checksums

  $EscapedAsset = [Regex]::Escape($Asset)
  $Line = Get-Content $Checksums | Where-Object { $_ -match "\s\*?$EscapedAsset$" } | Select-Object -First 1
  if (-not $Line) { Die "$Asset is missing from $ChecksumFile" }
  $Expected = ($Line -split '\s+')[0].ToLowerInvariant()
  $Actual = (Get-FileHash -Algorithm SHA256 -Path $Archive).Hash.ToLowerInvariant()
  if ($Actual -ne $Expected) { Die "checksum mismatch for $Asset" }

  $Extract = Join-Path $TempDir "extract"
  Expand-Archive -Path $Archive -DestinationPath $Extract -Force
  $Extracted = Join-Path $Extract $Package
  $Binary = Join-Path $Extracted "bin\godot-mcp.exe"
  if (-not (Test-Path -PathType Leaf $Binary)) { Die "$Asset does not contain $Package\bin\godot-mcp.exe" }
  & $Binary self-check --package $Extracted | Out-Null
  if ($LASTEXITCODE -ne 0) { Die "the extracted package failed its own self-check" }

  New-Item -ItemType Directory -Force -Path $InstallRoot | Out-Null
  $Destination = Join-Path $InstallRoot $Package
  if (Test-Path $Destination) { Remove-Item -Recurse -Force $Destination }
  Move-Item -Path $Extracted -Destination $Destination
  $BinDir = Join-Path $Destination "bin"
  Write-Host "Installed $Package to $Destination"

  if ($env:GODOTMCP_NO_PATH -ne "1") {
    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $Entries = @()
    if ($UserPath) {
      # Drop the bin directory of any earlier godot-mcp package, then add this one.
      $Entries = $UserPath -split ';' | Where-Object { $_ -and -not ($_ -like (Join-Path $InstallRoot "godot-mcp-*\bin")) }
    }
    $Entries = @($BinDir) + $Entries
    [Environment]::SetEnvironmentVariable("Path", ($Entries -join ';'), "User")
    Write-Host "Added $BinDir to the user PATH; open a new terminal to use godot-mcp"
  } else {
    Write-Host "Run $BinDir\godot-mcp.exe, or add $BinDir to PATH"
  }
} finally {
  Remove-Item -Recurse -Force $TempDir -ErrorAction SilentlyContinue
}
