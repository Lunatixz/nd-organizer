# Minimal local log-webhook catcher + viewer for nd-organizer.
#
# No third-party software and no admin rights. Raw TCP listener (no URL ACL).
#   POST <anything>  -> append body to webhook.log and the in-memory log
#   GET  /           -> render a clean, auto-refreshing status/activity page
#
# Usage:
#   pwsh ./scripts/webhook.ps1            # listen on port 8099
#   pwsh ./scripts/webhook.ps1 -Port 9000
#
# In Navidrome set the plugin's "Log webhook URL" to:
#   http://<this machine's LAN IP>:8099/
# then run a pass. Open that URL in a browser to watch the status stream in.

param(
    [int]$Port = 8099,
    [string]$LogFile = ""
)

$ErrorActionPreference = "Stop"

# Logs default to the folder this script lives in. To write them next to the
# plugin's data on a Navidrome host, pass -LogFile, e.g.:
#   pwsh .\webhook.ps1 -LogFile "\\my-nas\opt\navidrome\data\plugins\nd-organizer\webhook.log"
if ([string]::IsNullOrEmpty($LogFile)) {
    $LogFile = Join-Path $PSScriptRoot "webhook.log"
}

# Entries are objects: { ts, method, path, body }
$entries = New-Object System.Collections.Generic.List[object]

$ip = (Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue |
    Where-Object { $_.IPAddress -notlike "127.*" -and $_.IPAddress -notlike "169.254.*" } |
    Select-Object -First 1 -ExpandProperty IPAddress)

Write-Host "Listening on 0.0.0.0:${Port}"
Write-Host "Set the plugin's logWebhookUrl to:  http://${ip}:${Port}/"
Write-Host "Open that URL in a browser to view the log. Log file: $LogFile"
Write-Host "Press Ctrl+C to stop."

function Esc([string]$s) {
    $s -replace "&", "&amp;" -replace "<", "&lt;" -replace ">", "&gt;"
}

# Render a parsed status JSON as a compact summary card, or $null if not a status doc.
function Get-StatusCard([string]$body) {
    try { $j = $body | ConvertFrom-Json } catch { return $null }
    if ($null -eq $j -or $null -eq $j.mode) { return $null }

    $state = if ($j.inProgress) { "<span class='tag run'>RUNNING</span>" } elseif ($j.deferredUntilIdle) { "<span class='tag wait'>WAITING FOR IDLE</span>" } else { "<span class='tag ok'>IDLE</span>" }
    $mode = $j.mode
    $batch = ""
    if ($j.batch -and $j.batch.total -gt 0) {
        $batch = "<span class='tag'>batch $($j.batch.index + 1)/$($j.batch.total)</span>"
    }
    $ts = if ($j.ts) { (Get-Date -Date (([DateTimeOffset]::FromUnixTimeSeconds([int64]$j.ts)).LocalDateTime) -Format "HH:mm:ss") } else { "" }

    $html = "<div class='card'><h2>Status <span class='meta'>$ts</span></h2>"
    $html += "<div class='kv'>$state <span class='tag mode'>$mode</span> $batch"
    if ($j.rollbackOfRun) { $html += " <span class='tag'>rollback of $($j.rollbackOfRun)</span>" }
    $html += "</div>"

    if ($j.libraries -and $j.libraries.Count -gt 0) {
        $html += "<table><tr><th>Library</th><th>Albums found</th><th>To move</th><th>File moves</th><th>Kept</th><th>Skipped</th></tr>"
        foreach ($lib in $j.libraries) {
            $html += "<tr><td>$($lib.name) <span class='dim'>(id $($lib.id))</span></td><td>$($lib.albumsFound)</td><td><b>$($lib.albumsToMove)</b></td><td>$($lib.fileMoves)</td><td>$($lib.kept)</td><td>$($lib.skipped)</td></tr>"
        }
        $html += "</table>"
        $html += "<div class='totals'>Total to move: <b>$($j.totalAlbumsToMove)</b> &middot; file moves: <b>$($j.totalFileMoves)</b></div>"
    } elseif ($j.deferredUntilIdle) {
        $html += "<div class='note'>Run was deferred because playback is active. It will retry automatically.</div>"
    } else {
        $html += "<div class='note'>No libraries processed yet.</div>"
    }

    if ($j.warnings -and $j.warnings.Count -gt 0) {
        $html += "<div class='warn'><b>Warnings:</b><ul>"
        foreach ($w in $j.warnings) { $html += "<li>" + (Esc $w) + "</li>" }
        $html += "</ul></div>"
    }
    $html += "</div>"
    return $html
}

function Get-EntrySummary([string]$body) {
    try { $j = $body | ConvertFrom-Json } catch { return $null }
    if ($null -eq $j -or $null -eq $j.mode) { return $null }
    $parts = @($j.mode)
    if ($j.batch -and $j.batch.total -gt 0) { $parts += "batch $($j.batch.index + 1)/$($j.batch.total)" }
    if ($j.deferredUntilIdle) { $parts += "deferred (idle)" }
    if ($j.libraries -and $j.libraries.Count -gt 0) {
        $lib = $j.libraries[0]
        $parts += "$($lib.albumsToMove) to move"
        $parts += "$($lib.fileMoves) file moves"
    }
    return ($parts -join " | ")
}

$listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Any, $Port)
$listener.Start()

try {
    while ($true) {
        $client = $listener.AcceptTcpClient()
        try {
            $stream = $client.GetStream()
            $reader = [System.IO.StreamReader]::new($stream, [System.Text.Encoding]::UTF8)

            $requestLine = $reader.ReadLine()
            if ([string]::IsNullOrEmpty($requestLine)) { $client.Close(); continue }

            $method = ($requestLine -split " ")[0]
            $contentLength = 0
            while (($line = $reader.ReadLine()) -ne $null) {
                if ($line -eq "") { break }
                if ($line -match "^Content-Length:\s*(\d+)") { $contentLength = [int]$matches[1] }
            }

            $body = ""
            if ($contentLength -gt 0) {
                $buf = New-Object char[] $contentLength
                $total = 0
                $stream.ReadTimeout = 10000
                while ($total -lt $contentLength) {
                    $n = $reader.Read($buf, $total, $contentLength - $total)
                    if ($n -le 0) { break }
                    $total += $n
                }
                $body = -join $buf[0..($total - 1)]
            }

            $ts = Get-Date -Format "yyyy-MM-dd HH:mm:ss"

            if ($method -eq "POST") {
                $path = ($requestLine -replace ' HTTP/.*$', '' -replace '^POST ', '')
                $entry = [PSCustomObject]@{ ts = $ts; method = "POST"; path = $path; body = $body }
                $entries.Add($entry)
                Add-Content -Path $LogFile -Value "[$ts] POST $path - $body"
                Write-Host "[$ts] POST $path - $(if ($body.Length -gt 120) { $body.Substring(0,120) + '...' } else { $body })"

                $resp = "HTTP/1.1 200 OK`r`nContent-Length: 3`r`nConnection: close`r`n`r`nok`n"
                $bytes = [System.Text.Encoding]::ASCII.GetBytes($resp)
                $stream.Write($bytes, 0, $bytes.Length)
                $client.Close()
                continue
            }

            # GET: render the status + activity page.
            $statusCard = ""
            for ($i = $entries.Count - 1; $i -ge 0; $i--) {
                $card = Get-StatusCard $entries[$i].body
                if ($card) { $statusCard = $card; break }
            }

            $rows = ""
            for ($i = $entries.Count - 1; $i -ge 0; $i--) {
                $e = $entries[$i]
                $summary = Get-EntrySummary $e.body
                $rows += "<div class='e'><span class='ts'>$($e.ts)</span> <span class='m'>$($e.method)</span> <span class='p'>$(Esc $e.path)</span>"
                if ($summary) {
                    $rows += "<div class='sum'>$(Esc $summary)</div>"
                    $rows += "<details><summary>raw json</summary><pre>" + (Esc $e.body) + "</pre></details>"
                } else {
                    $rows += "<details open><summary>report / log</summary><pre>" + (Esc $e.body) + "</pre></details>"
                }
                $rows += "</div>"
            }
            if ($rows -eq "") { $rows = "<div class='note'>Waiting for the plugin to POST its status/reports &hellip;</div>" }

            $html = @"
<!DOCTYPE html><html><head><meta charset="utf-8"><meta http-equiv="refresh" content="5">
<title>nd-organizer</title>
<style>
body{background:#0e1117;color:#d7dde6;font:14px/1.5 -apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif;margin:0;padding:20px;max-width:1000px;margin:0 auto}
h1{font-size:18px;color:#8ab4f8;margin:0 0 4px}
.sub{color:#8b93a5;font-size:12px;margin-bottom:16px}
.card{background:#161b24;border:1px solid #232a36;border-radius:10px;padding:14px 16px;margin-bottom:20px}
h2{font-size:15px;margin:0 0 10px;color:#e6eaf1}
h2 .meta{color:#8b93a5;font-size:12px;font-weight:normal}
.kv{display:flex;gap:8px;flex-wrap:wrap;align-items:center;margin-bottom:10px}
.tag{background:#232a36;border-radius:12px;padding:2px 10px;font-size:12px;color:#c8d0db}
.tag.run{background:#6b3a00;color:#ffcf8a}
.tag.wait{background:#3a2c00;color:#ffd98a}
.tag.ok{background:#0f3d24;color:#8ff0b5}
.tag.mode{background:#1d3a5f;color:#9cc8ff}
table{width:100%;border-collapse:collapse;font-size:13px;margin:6px 0}
th{text-align:left;color:#8b93a5;font-weight:500;padding:4px 8px;border-bottom:1px solid #232a36}
td{padding:4px 8px;border-bottom:1px solid #1c2230}
.dim{color:#8b93a5;font-size:12px}
.totals{margin-top:8px;color:#c8d0db}
.note{color:#8b93a5;font-size:13px}
.warn{background:#2a1f12;border:1px solid #5c4a1e;border-radius:8px;padding:8px 12px;margin-top:10px;color:#ffd9a0;font-size:13px}
.warn ul{margin:6px 0 0;padding-left:18px}
.e{background:#161b24;border:1px solid #232a36;border-radius:8px;padding:10px 14px;margin-bottom:10px}
.e .ts{color:#8b93a5;font-size:12px}
.e .m{background:#1d3a5f;color:#9cc8ff;border-radius:4px;padding:1px 6px;font-size:11px}
.e .p{color:#aab6c5;font-size:12px;margin-left:6px}
.e .sum{color:#9be0a6;font-size:13px;margin:6px 0 4px}
details summary{cursor:pointer;color:#8b93a5;font-size:12px}
pre{white-space:pre-wrap;word-break:break-word;background:#0e1117;border:1px solid #1c2230;border-radius:6px;padding:8px;font:12px/1.4 Consolas,monospace;color:#c8d0db;max-height:320px;overflow:auto;margin:6px 0 0}
</style></head><body>
<h1>nd-organizer</h1>
<div class="sub">$($entries.Count) events &middot; auto-refresh 5s &middot; log: $LogFile</div>
$statusCard
$rows
</body></html>
"@
            $bytes = [System.Text.Encoding]::UTF8.GetBytes($html)
            $resp = "HTTP/1.1 200 OK`r`nContent-Type: text/html; charset=utf-8`r`nContent-Length: $($bytes.Length)`r`nConnection: close`r`n`r`n"
            $head = [System.Text.Encoding]::ASCII.GetBytes($resp)
            $stream.Write($head, 0, $head.Length)
            $stream.Write($bytes, 0, $bytes.Length)
            $client.Close()
            continue
        }
        catch {
            Write-Host "request error: $($_.Exception.Message)"
        }
        finally {
            $client.Close()
        }
    }
}
finally {
    $listener.Stop()
    Write-Host "Stopped."
}
