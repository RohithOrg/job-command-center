# Run Job Command Center from source with one command (Windows).
# Installs the Rust toolchain if it is missing. The first build takes a few minutes.
#   powershell -ExecutionPolicy Bypass -File scripts\start.ps1
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
    if (Test-Path (Join-Path $cargoBin "cargo.exe")) {
        $env:Path = "$cargoBin;$env:Path"
    } else {
        Write-Host "Installing the Rust toolchain (rustup)..."
        $tmp = Join-Path $env:TEMP "rustup-init.exe"
        Invoke-WebRequest -Uri "https://win.rustup.rs/x86_64" -OutFile $tmp
        & $tmp -y --profile minimal
        $env:Path = "$cargoBin;$env:Path"
    }
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "cargo not found. Open a new terminal and run this script again."
}

Write-Host "Building and starting Job Command Center (the first build takes a few minutes)..."
cargo run --release --locked -- @args
