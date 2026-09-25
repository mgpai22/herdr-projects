# Puts the herdr-projects binary at target\release\herdr-projects.exe (Windows).
#
# Herdr runs this as the plugin's build step on Windows. There are no Windows
# release binaries, so it always runs the locked Cargo release build. The
# target directory is pinned to target\ because herdr-plugin.toml's commands
# run target/release/herdr-projects, whatever CARGO_TARGET_DIR says.
# Continue: under 'Stop', Windows PowerShell 5.1 turns cargo's redirected
# stderr progress lines into terminating errors.
$ErrorActionPreference = 'Continue'
Set-Location (Split-Path -Parent $PSScriptRoot)

function Say([string]$text) { [Console]::Error.WriteLine("herdr-projects install: $text") }

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Say 'cargo is not installed. Install Rust 1.89 or newer (https://rustup.rs) with the MSVC build tools, then install again.'
    exit 1
}
Say 'building from source: cargo build --release --locked (this takes a minute or two)'
& cargo build --release --locked --target-dir target
exit $LASTEXITCODE
