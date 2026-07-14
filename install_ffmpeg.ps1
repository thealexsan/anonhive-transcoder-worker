# FFmpeg Auto-Installer for Windows (supports GPU-accelerated NVENC)
# Run this script as Administrator in PowerShell.

Write-Host "=== Anonhive FFmpeg Installer ===" -ForegroundColor Cyan

# 1. Check if FFmpeg is already installed
$ffmpegExists = Get-Command ffmpeg -ErrorAction SilentlyContinue
if ($ffmpegExists) {
    Write-Host "✅ FFmpeg is already installed and available in your PATH." -ForegroundColor Green
    exit 0
}

# 2. Try installing via winget
Write-Host "Attempting to install Gyan.FFmpeg via winget..." -ForegroundColor Yellow
try {
    winget install Gyan.FFmpeg --accept-source-agreements --accept-package-agreements --silent
    if ($LASTEXITCODE -eq 0) {
        Write-Host "✅ FFmpeg installed successfully via winget!" -ForegroundColor Green
        Write-Host "👉 Please RESTART your PowerShell window for the path changes to take effect." -ForegroundColor Cyan
        exit 0
    }
} catch {
    Write-Host "winget failed or is not available. Falling back to manual download..." -ForegroundColor Yellow
}

# 3. Manual Fallback: Download and extract from gyan.dev
$destFolder = "C:\ffmpeg"
$zipPath = "$env:TEMP\ffmpeg.zip"
$downloadUrl = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-git-full.7z"

# Note: gyan.dev uses .7z which is compact. We will download the essentials zip build instead to ensure native PowerShell Expand-Archive works without needing 7-Zip installed on the system.
$downloadUrl = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip"

Write-Host "Downloading FFmpeg from gyan.dev..." -ForegroundColor Yellow
try {
    Invoke-WebRequest -Uri $downloadUrl -OutFile $zipPath -UserAgent "Mozilla/5.0"
} catch {
    Write-Error "Failed to download FFmpeg zip: $_"
    exit 1
}

Write-Host "Extracting FFmpeg to $destFolder..." -ForegroundColor Yellow
if (Test-Path $destFolder) {
    Remove-Item -Path $destFolder -Recurse -Force -ErrorAction SilentlyContinue
}
New-Item -ItemType Directory -Path $destFolder -Force | Out-Null

try {
    Expand-Archive -Path $zipPath -DestinationPath $destFolder -Force
    # Move files out of the subfolder created by Expand-Archive to C:\ffmpeg directly
    $subfolder = Get-ChildItem -Path $destFolder -Directory | Select-Object -First 1
    if ($subfolder) {
        Move-Item -Path "$($subfolder.FullName)\*" -Destination $destFolder -Force
        Remove-Item -Path $subfolder.FullName -Recurse -Force
    }
} catch {
    Write-Error "Failed to extract FFmpeg: $_"
    exit 1
}

# Add C:\ffmpeg\bin to the User Environment PATH
Write-Host "Adding C:\ffmpeg\bin to User PATH..." -ForegroundColor Yellow
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*C:\ffmpeg\bin*") {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;C:\ffmpeg\bin", "User")
    Write-Host "✅ Successfully added C:\ffmpeg\bin to User PATH!" -ForegroundColor Green
} else {
    Write-Host "C:\ffmpeg\bin is already in User PATH." -ForegroundColor Green
}

# Cleanup zip
Remove-Item -Path $zipPath -Force -ErrorAction SilentlyContinue

Write-Host "=============================================" -ForegroundColor Cyan
Write-Host "✅ Installation Complete!" -ForegroundColor Green
Write-Host "👉 Please RESTART your PowerShell window for the path changes to take effect." -ForegroundColor Cyan
Write-Host "=============================================" -ForegroundColor Cyan
