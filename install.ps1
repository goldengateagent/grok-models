$ErrorActionPreference = "Stop"

$Repo = "goldengateagent/grok-models"
$Version = "1.0.0"
$Artifact = "grok-models"

$InstallDir = "$env:LOCALAPPDATA\grok-models"
$BinDir = "$InstallDir\bin"

# Detect architecture
$Arch = $env:PROCESSOR_ARCHITECTURE
if ($Arch -eq "ARM64") {
    $Target = "aarch64-pc-windows-msvc"
} elseif ($Arch -eq "AMD64") {
    $Target = "x86_64-pc-windows-msvc"
} else {
    Write-Error "Unsupported architecture: $Arch"
    exit 1
}

$File = "$Artifact-$Version-$Target.zip"
$Url = "https://github.com/$Repo/releases/download/v$Version/$File"

$Tmp = [System.IO.Path]::GetTempPath()
$ZipFile = Join-Path $Tmp "$File"

Write-Host "Downloading $File..."
try {
    Invoke-WebRequest -Uri $Url -OutFile $ZipFile -UseBasicParsing
} catch {
    Write-Error "Failed to download $Url"
    exit 1
}

# Extract zip
Write-Host "Extracting..."
Expand-Archive -Path $ZipFile -DestinationPath $Tmp -Force

$Stage = Join-Path $Tmp "$Artifact-$Version-$Target"
$ExePath = Join-Path $Stage "$Artifact.exe"
if (-not (Test-Path $ExePath)) {
    Write-Error "Expected binary not found at $ExePath"
    exit 1
}

New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
Copy-Item $ExePath "$BinDir\$Artifact.exe" -Force
Remove-Item $ZipFile -Force
Remove-Item $Stage -Recurse -Force

# Add to user PATH
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($UserPath -notlike "*$BinDir*") {
    [Environment]::SetEnvironmentVariable(
        "Path",
        "$UserPath;$BinDir",
        "User"
    )
    Write-Host "Added $BinDir to user PATH."
    Write-Host "Restart your shell or run: `$env:PATH += `";$BinDir`""
} else {
    Write-Host "$BinDir already in PATH."
}

Write-Host
Write-Host "Installed to: $BinDir\$Artifact.exe"