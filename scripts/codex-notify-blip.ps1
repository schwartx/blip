param(
    [Parameter(Position = 0)]
    [string] $EventJson
)

$logPath = Join-Path $PSScriptRoot 'notify-blip.log'

function Write-Diagnostic([string] $Message) {
    try {
        Add-Content -LiteralPath $logPath -Encoding UTF8 -Value `
            ("{0:o} pid={1} {2}" -f [DateTime]::Now, $PID, $Message)
    } catch {
        # Diagnostics must not interfere with notification delivery.
    }
}

# Codex appends one JSON event argument to the configured notify command.
if ([string]::IsNullOrWhiteSpace($EventJson)) {
    Write-Diagnostic 'ignored: missing event argument'
    exit 0
}

try {
    $event = $EventJson | ConvertFrom-Json -ErrorAction Stop
    Write-Diagnostic ("received: type={0}" -f $event.type)
    if ($event.type -ne 'agent-turn-complete') {
        Write-Diagnostic 'ignored: unsupported event type'
        exit 0
    }

    $messageProperty = $event.PSObject.Properties['last-assistant-message']
    $message = if ($messageProperty) { $messageProperty.Value } else { $null }
    if ($message -isnot [string] -or $message.Length -eq 0) {
        $message = 'Task completed'
    } elseif ($message.Length -gt 2000) {
        $message = $message.Substring(0, 1997) + '...'
    }

    $cwd = [string] $event.cwd
    $project = ($cwd.TrimEnd('/', '\') -split '[/\\]')[-1]
    if ([string]::IsNullOrWhiteSpace($project)) {
        $project = 'Unknown project'
    }

    $payload = [ordered]@{
        title  = "Codex [Win] - $project"
        body   = $message
        level  = 'normal'
        source = 'codex-win'
    }

    $threadProperty = $event.PSObject.Properties['thread-id']
    $threadId = if ($threadProperty) { $threadProperty.Value } else { $null }
    if ($threadId -is [string] -and $threadId.Length -gt 0) {
        $payload.id = "codex-$threadId"
    }

    $baseUrl = if ($env:BLIP_URL) { $env:BLIP_URL.TrimEnd('/') } else { 'http://127.0.0.1:7788' }
    $json = $payload | ConvertTo-Json -Compress
    Add-Type -AssemblyName System.Net.Http
    $handler = [System.Net.Http.HttpClientHandler]::new()
    $handler.UseProxy = $false
    $client = [System.Net.Http.HttpClient]::new($handler)
    $client.Timeout = [TimeSpan]::FromSeconds(3)
    $content = [System.Net.Http.StringContent]::new(
        $json,
        [System.Text.Encoding]::UTF8,
        'application/json'
    )
    try {
        $response = $client.PostAsync("$baseUrl/notify", $content).GetAwaiter().GetResult()
        $response.EnsureSuccessStatusCode() | Out-Null
        Write-Diagnostic ("delivered: status={0} project={1}" -f ([int] $response.StatusCode), $project)
        $response.Dispose()
    } finally {
        $content.Dispose()
        $client.Dispose()
        $handler.Dispose()
    }
} catch {
    Write-Diagnostic ("failed: {0}" -f $_.Exception.Message)
    # Notification delivery is best-effort and must never disrupt Codex.
}

exit 0
