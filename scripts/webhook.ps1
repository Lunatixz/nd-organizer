# Minimal local log-webhook catcher + viewer for nd-organizer.
#
# No third-party software and no admin rights. Raw TCP listener (no URL ACL).
#   POST <anything>  -> append body to webhook.log and the in-memory log
#   GET  /           -> render the accumulated log/status as an auto-refreshing
#                       HTML page, so the URL actually loads the status.
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

# Logs go to the Navidrome plugin folder (where the plugin's own data lives),
# not the GitHub source folder. Fall back to the local scripts dir if the NAS
# share isn't reachable.
if ([string]::IsNullOrEmpty($LogFile)) {
    $ndPluginDir = "\\192.168.0.21\opt\navidrome\data\plugins\nd-organizer"
    if (Test-Path $ndPluginDir) {
        $LogFile = Join-Path $ndPluginDir "webhook.log"
    } else {
        $LogFile = Join-Path $PSScriptRoot "webhook.log"
    }
}

$entries = New-Object System.Collections.Generic.List[string]

$ip = (Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue |
    Where-Object { $_.IPAddress -notlike "127.*" -and $_.IPAddress -notlike "169.254.*" } |
    Select-Object -First 1 -ExpandProperty IPAddress)

Write-Host "Listening on 0.0.0.0:${Port}"
Write-Host "Set the plugin's logWebhookUrl to:  http://${ip}:${Port}/"
Write-Host "Open that URL in a browser to view the log. Log file: $logFile"
Write-Host "Press Ctrl+C to stop."

# Load any previously captured entries.
if (Test-Path $logFile) {
    Get-Content $logFile -ErrorAction SilentlyContinue | ForEach-Object { $entries.Add($_) }
}

function Esc([string]$s) {
    $s -replace "&", "&amp;" -replace "<", "&lt;" -replace ">", "&gt;"
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
                $line = "[$ts] $($requestLine -replace ' HTTP/.*$','') - $body"
                $entries.Add($line)
                Add-Content -Path $logFile -Value $line
                Write-Host $line
                $resp = "HTTP/1.1 200 OK`r`nContent-Length: 3`r`nConnection: close`r`n`r`nok`n"
                $bytes = [System.Text.Encoding]::ASCII.GetBytes($resp)
            }
            else {
                # GET: render the accumulated log as a refreshing HTML page.
                $rows = ($entries | ForEach-Object { "<div class='e'>" + (Esc $_) + "</div>" }) -join "`n"
                $html = @"
<!DOCTYPE html><html><head><meta charset="utf-8"><meta http-equiv="refresh" content="5">
<title>nd-organizer log webhook</title>
<style>
body{background:#111;color:#ddd;font:13px/1.5 Consolas,monospace;margin:20px}
h1{font-size:16px;color:#8ab4f8}
#count{color:#999}
.e{white-space:pre-wrap;border-bottom:1px solid #222;padding:2px 0}
.e:first-letter{}
</style></head><body>
<h1>nd-organizer log webhook</h1>
<div id="count">$($entries.Count) entries &mdash; auto-refresh 5s. Waiting for the plugin to POST its report/status&hellip;</div>
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
            $stream.Write($bytes, 0, $bytes.Length)
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
