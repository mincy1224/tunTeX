param(
    [string]$Client = 'D:/Programs/tunTeX/tuntex-client.exe',
    [string]$Document = 'E:/Reading/pre2610/main.tex',
    [int]$Runs = 5
)
$ErrorActionPreference = 'Stop'
$directory = Split-Path -Parent $Document
$arguments = @('-xelatex', '-synctex=1', '-interaction=nonstopmode', '-file-line-error', '-halt-on-error', '-cd', "-outdir=$directory", "-auxdir=$directory/build", $Document)
foreach ($run in 1..$Runs) {
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $output = & $Client @arguments 2>&1
    $code = $LASTEXITCODE
    $watch.Stop()
    [pscustomobject]@{Mode='normal';Run=$run;Milliseconds=$watch.ElapsedMilliseconds;ExitCode=$code}
    if ($code -ne 0) { $output | Select-Object -Last 12; break }
}
$watch = [Diagnostics.Stopwatch]::StartNew()
$output = & $Client '-g' @arguments 2>&1
$code = $LASTEXITCODE
$watch.Stop()
[pscustomobject]@{Mode='forced';Run=1;Milliseconds=$watch.ElapsedMilliseconds;ExitCode=$code}
if ($code -ne 0) { $output | Select-Object -Last 12 }
