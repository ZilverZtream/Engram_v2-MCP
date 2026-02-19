$testDir = 'C:\ai-projects\Engram-MCP_v2\crates\engram_server\tests'
$files = Get-ChildItem -Path $testDir -Filter '*.rs' -Recurse

foreach ($file in $files) {
    $content = Get-Content $file.FullName -Raw
    $insert = "        ..Default::default()"
    # Split on the closing }; pattern that follows max_concurrent_jobs
    $pattern = '(        max_concurrent_jobs: \d+,)(\r?\n)(    \};)'
    $replacement = '$1$2' + $insert + '$2$3'
    $newContent = $content -replace $pattern, $replacement
    if ($newContent -ne $content) {
        Set-Content -Path $file.FullName -Value $newContent -NoNewline
        Write-Host "Updated: $($file.FullName)"
    }
}
