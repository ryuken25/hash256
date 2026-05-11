# PowerShell launcher buat coordinator (Windows host).
# Pastikan .env berada di working dir.

$ErrorActionPreference = "Stop"
Set-Location -Path (Split-Path -Parent $PSScriptRoot)

if (-not (Test-Path ".env")) {
    Write-Error ".env tidak ditemukan di $(Get-Location). Copy dari .env.example dulu."
    exit 1
}

if (-not (Test-Path ".\target\release\hash256.exe")) {
    Write-Host "binary belum di-build, jalanin: cargo build --release"
    cargo build --release
}

Write-Host "==============================================="
Write-Host "Coordinator URL buat worker (Tailscale):"
Write-Host "  http://100.121.79.5:8787"
Write-Host "Auth token harus sama di .env.worker:"
Write-Host "  WORKER_AUTH_TOKEN dari .env"
Write-Host "==============================================="
Write-Host ""

.\target\release\hash256.exe coordinator
