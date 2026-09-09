# Push a local file into a running Docker container via Portainer API + restart.
#
# Usage:
#   pwsh ./scripts/push-container.ps1 -Container "nd-organizer-webhook" -LocalFile "webhook/server.py" -RemotePath "/app/server.py"
#   pwsh ./scripts/push-container.ps1 -Container "nd-organizer-acoustid" -LocalFile "acoustid/server.py" -RemotePath "/app/server.py"

param(
    [Parameter(Mandatory)][string]$Container,
    [Parameter(Mandatory)][string]$LocalFile,
    [Parameter(Mandatory)][string]$RemotePath,
    [string]$PortainerUrl = "http://192.168.0.21:9000",
    [string]$ApiKey = "ptr_ZYi4rjc6DQIzAH3joO8th827Rxq38vE2b9NjKQPPkrQ=",
    [string]$EndpointId = "13"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

# Find container ID
$containers = curl.exe -s "$PortainerUrl/api/endpoints/$EndpointId/docker/containers/json" -H "X-API-Key: $ApiKey" | python -c "import sys,json; print(json.dumps(json.loads(sys.stdin.read())))"
$cid = ($containers | python -c "import sys,json; [print(c['Id']) for c in json.loads(sys.stdin.read()) if '$Container' in c['Names'][0]]")
if (-not $cid) { throw "Container '$Container' not found" }
Write-Host "Container: $Container ($($cid.Substring(0,12)))"

# Base64 encode the file
$filePath = Join-Path $root $LocalFile
$b64 = [Convert]::ToBase64String([System.IO.File]::ReadAllBytes($filePath))
Write-Host "File: $LocalFile ($(([System.IO.File]::ReadAllBytes($filePath)).Length) bytes)"

# Create exec to write file via base64
$execJson = @{
    AttachStdout = $true
    AttachStderr = $true
    Cmd = @("sh", "-c", "echo '$b64' | base64 -d > $RemotePath && echo OK")
} | ConvertTo-Json -Depth 3

$resp1 = curl.exe -s -X POST "$PortainerUrl/api/endpoints/$EndpointId/docker/containers/$cid/exec" -H "X-API-Key: $ApiKey" -H "Content-Type: application/json" -d $execJson
$execId = ($resp1 | python -c "import sys,json; print(json.loads(sys.stdin.read()).get('Id',''))")
if (-not $execId) { throw "Failed to create exec: $resp1" }

$resp2 = curl.exe -s -X POST "$PortainerUrl/api/endpoints/$EndpointId/docker/exec/$execId/start" -H "X-API-Key: $ApiKey" -H "Content-Type: application/json" -d '{"Detach":false,"Tty":false}'
Write-Host "Exec: $resp2"

# Restart container
curl.exe -s -X POST "$PortainerUrl/api/endpoints/$EndpointId/docker/containers/$cid/restart" -H "X-API-Key: $ApiKey" | Out-Null
Start-Sleep 5
Write-Host "Restarted $Container"
