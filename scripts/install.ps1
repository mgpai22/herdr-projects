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

# The binary embeds these files byte for byte. A clone made with
# core.autocrlf=true before .gitattributes existed keeps them with CRLF.
if (Get-Command git -ErrorAction SilentlyContinue) {
    $crlf = @(& git ls-files --eol -- assets/omp skill 2>$null | Where-Object { $_ -match '\sw/crlf\s' } | ForEach-Object { ($_ -split "`t")[-1] })
    if ($crlf.Count -gt 0) {
        Say 'these files have Windows (CRLF) line endings in this checkout, and the binary would embed them:'
        $crlf | ForEach-Object { Say "  $_" }
        $list = ($crlf | ForEach-Object { "'$_'" }) -join ','
        Say "Rewrite them once with LF endings (this discards local edits to them), then install again:"
        Say "  Remove-Item $list; git checkout -- $($list -replace ',', ' ')"
        exit 1
    }
}

# Windows cannot replace a running exe, and the ticker, the tab-bar command
# and the popups run this one. It can rename it: the old copy runs on, the
# build writes a new one. Earlier copies are deleted once nothing runs them.
$exe = 'target\release\herdr-projects.exe'
Get-ChildItem 'target\release' -Filter 'herdr-projects.exe.*.old' -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
$old = $null
if (Test-Path $exe) {
    $old = "$exe.$([DateTime]::UtcNow.Ticks).old"
    Move-Item $exe $old -ErrorAction SilentlyContinue
    if (-not $?) { $old = $null }
}

Say 'building from source: cargo build --release --locked (this takes a minute or two)'
& cargo build --release --locked --target-dir target
$code = $LASTEXITCODE
if ($old -and -not (Test-Path $exe)) {
    Move-Item $old $exe
}
exit $code
