param([Parameter(Mandatory=$true)][string]$Chromium, [string]$OutputDirectory)
$ErrorActionPreference = 'Stop'
if (-not $OutputDirectory) { $OutputDirectory = $PSScriptRoot }
$browser = (Resolve-Path -LiteralPath $Chromium).Path
[System.IO.Directory]::CreateDirectory($OutputDirectory) | Out-Null
$OutputDirectory = [System.IO.Path]::GetFullPath($OutputDirectory)
$profile = [IO.Path]::Combine($OutputDirectory, ('chromium-profile-' + [Guid]::NewGuid().ToString('N')))
$font = [IO.Path]::Combine($PSScriptRoot, '../../../lumen-html-text/fonts/LiberationSans-Regular.ttf')
function Hash-File([string]$path) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { return [BitConverter]::ToString($sha.ComputeHash([System.IO.File]::ReadAllBytes($path))).Replace('-', '').ToLowerInvariant() }
    finally { $sha.Dispose() }
}
foreach ($name in @('geometry','flex','text','paint','wrap','width')) {
    $fixture = [IO.Path]::Combine($PSScriptRoot, "$name.html")
    $output = [IO.Path]::Combine($OutputDirectory, "$name.png")
    $url = ([System.Uri]$fixture).AbsoluteUri
    $started = [DateTime]::UtcNow
    $process = Start-Process -FilePath $browser -ArgumentList @('--headless=new', '--no-sandbox', '--disable-gpu', '--hide-scrollbars', '--no-first-run', '--no-default-browser-check', '--allow-file-access-from-files', "--user-data-dir=`"$profile`"", '--force-device-scale-factor=1', '--window-size=64,64', '--virtual-time-budget=1000', "--screenshot=`"$output`"", "`"$url`"") -WindowStyle Hidden -Wait -PassThru
    if ($process.ExitCode -ne 0 -or -not (Test-Path -LiteralPath $output) -or (Get-Item -LiteralPath $output).LastWriteTimeUtc -lt $started) { throw "Chromium did not produce $name reference image" }
    @{
    chromium = (Get-Item -LiteralPath $browser).VersionInfo.ProductVersion
    viewport = @(64,64)
    scale = 1
    font_sha256 = Hash-File $font
    fixture_sha256 = Hash-File $fixture
    } | ConvertTo-Json | Set-Content -LiteralPath ([IO.Path]::Combine($OutputDirectory, "$name.json")) -Encoding UTF8
}
$profile = [IO.Path]::GetFullPath($profile)
if ([IO.Path]::GetDirectoryName($profile) -ne $OutputDirectory) { throw 'Chromium profile escaped the reference output directory' }
if (Test-Path -LiteralPath $profile) { Remove-Item -LiteralPath $profile -Recurse -Force }
