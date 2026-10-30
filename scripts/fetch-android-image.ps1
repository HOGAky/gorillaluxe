# GorillaLuxe Android runtime bootstrapper (Phase 4).
# Resumable chunked downloader + extractor for Google's official
# Android repository packages. Progress is written as a single byte
# count to "$OutDir\.dl_progress" for the GUI to poll.
#
#   powershell -File fetch-android-image.ps1 `
#       -OutDir "$env:LOCALAPPDATA\GorillaLuxe\runtime" `
#       -Urls "https://dl.google.com/.../emu.zip;https://.../tools.zip;https://.../x86_64-34_r12.zip"
param(
    [Parameter(Mandatory = $true)][string]$OutDir,
    [Parameter(Mandatory = $true)][string]$Urls,
    [int]$ChunkBytes = 8MB
)
$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$progFile = Join-Path $OutDir '.dl_progress'
$doneGlobal = 0L
Set-Content -Path $progFile -Value '0'

function Get-Length([string]$url) {
    try {
        $req = [System.Net.HttpWebRequest]::Create($url)
        $req.Method = 'HEAD'
        $req.Timeout = 30000
        $resp = $req.GetResponse()
        $len = $resp.ContentLength
        $resp.Close()
        return $len
    } catch { return -1 }
}

function Download-Resumable([string]$url, [string]$file) {
    $part = "$file.part"
    $total = Get-Length $url
    $have = 0L
    if (Test-Path $file) { return }                     # already done
    if (Test-Path $part) { $have = (Get-Item $part).Length }
    $fs = [System.IO.File]::Open($part, 'OpenOrCreate', 'Write')
    $fs.Seek($have, 'Begin') | Out-Null
    try {
        while ($total -lt 0 -or $have -lt $total) {
            $req = [System.Net.HttpWebRequest]::Create($url)
            $req.Timeout = 60000
            if ($total -gt 0) {
                $end = [Math]::Min($have + $ChunkBytes - 1, $total - 1)
                $req.AddRange($have, $end)
            }
            try {
                $resp = $req.GetResponse()
            } catch {
                Start-Sleep -Milliseconds 1500
                continue                                 # resume on next iteration
            }
            $stream = $resp.GetResponseStream()
            $buf = New-Object byte[] 256KB
            while (($n = $stream.Read($buf, 0, $buf.Length)) -gt 0) {
                $fs.Write($buf, 0, $n)
                $have += $n
                $script:doneGlobal += $n
                $fs.Flush()
                Set-Content -Path $progFile -Value $script:doneGlobal
            }
            $stream.Close(); $resp.Close()
        }
    } finally { $fs.Close() }
    Move-Item -Force $part $file
}

function Expand-Runtime([string]$file, [string]$dest) {
    $dest | Split-Path -Parent | ForEach-Object { New-Item -ItemType Directory -Force -Path $_ | Out-Null }
    Expand-Archive -Force -Path $file -DestinationPath $dest
    Remove-Item -Force $file
}

foreach ($u in ($Urls -split ';')) {
    if ([string]::IsNullOrWhiteSpace($u)) { continue }
    $leaf = [uri]::UnescapeDataString((Split-Path $u -Leaf))
    $file = Join-Path $OutDir $leaf
    Write-Host "GORILLALUXE-DL: $leaf"
    Download-Resumable $u $file

    # Route each artifact to its runtime slot.
    if ($leaf -match 'emulator.*windows.*\.zip$') {
        Expand-Runtime $file $OutDir                      # -> emulator\emulator.exe
    } elseif ($leaf -match 'platform-tools.*\.zip$') {
        Expand-Runtime $file $OutDir                      # -> platform-tools\adb.exe
    } elseif ($leaf -match 'x86_64-(\d+)_r\d+\.zip$') {
        $api = $Matches[1]
        Expand-Runtime $file (Join-Path $OutDir "system-images\android-$api\google_apis_playstore\x86_64")
    } else {
        Expand-Runtime $file (Join-Path $OutDir 'packages')
    }
}
Remove-Item -Force $progFile -ErrorAction SilentlyContinue
Write-Host 'GORILLALUXE-DL: COMPLETE'
