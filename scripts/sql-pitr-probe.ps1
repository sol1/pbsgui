<#
.SYNOPSIS
  Generate and verify time markers for testing SQL point-in-time restore.

.DESCRIPTION
  Inserts a timestamped row into dbo.pbsgui_probe every few seconds, printing
  each marker's UTC time so you have a ground-truth timeline. Run it while a
  pbsgui point-in-time job is taking fulls and logs. Later, restore the database
  to a chosen moment T and run this with -Verify -AtUtc T against the restored
  copy: a correct restore has markers up to ~T and ZERO after T.

.EXAMPLE
  # Generate markers every 15s into PbsTestDb (Windows auth)
  .\sql-pitr-probe.ps1 -Server stanley -Database PbsTestDb -IntervalSeconds 15

.EXAMPLE
  # After restoring to 2026-06-22 11:44:00 UTC into PbsTestDb_pit, verify:
  .\sql-pitr-probe.ps1 -Server stanley -Database PbsTestDb_pit -Verify -AtUtc "2026-06-22T11:44:00"
#>
param(
  [string]$Server = "localhost",
  [string]$Database = "PbsTestDb",
  [int]$IntervalSeconds = 15,
  [switch]$Verify,
  # Target moment (UTC, "yyyy-MM-ddTHH:mm:ss") to check a point-in-time restore against.
  [string]$AtUtc,
  # SQL login; omit for Windows integrated auth.
  [string]$User,
  [string]$Password
)

$ErrorActionPreference = "Stop"
$auth = if ($User) { @("-U", $User, "-P", $Password) } else { @("-E") }

function Sql([string]$query) {
  & sqlcmd -S $Server -d $Database @auth -b -h -1 -Q $query
  if ($LASTEXITCODE -ne 0) { throw "sqlcmd failed ($LASTEXITCODE)" }
}

if ($Verify) {
  Write-Host "Marker summary in [$Database].dbo.pbsgui_probe:"
  Sql "SELECT COUNT(*) AS total, MIN(marker_utc) AS first_utc, MAX(marker_utc) AS last_utc FROM dbo.pbsgui_probe;"
  if ($AtUtc) {
    Write-Host ""
    Write-Host "Markers AFTER $AtUtc UTC (should be 0 for a clean restore to that moment):"
    Sql "SELECT COUNT(*) AS after_target FROM dbo.pbsgui_probe WHERE marker_utc > '$AtUtc';"
  }
  return
}

Sql @"
IF OBJECT_ID('dbo.pbsgui_probe') IS NULL
  CREATE TABLE dbo.pbsgui_probe (
    id INT IDENTITY PRIMARY KEY,
    marker_utc DATETIME2(0) NOT NULL DEFAULT SYSUTCDATETIME()
  );
"@

Write-Host "Inserting a marker every ${IntervalSeconds}s into [$Database].dbo.pbsgui_probe."
Write-Host "Note a UTC time mid-run to restore to later. Ctrl+C to stop."
while ($true) {
  Sql "INSERT dbo.pbsgui_probe DEFAULT VALUES;"
  $utc = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ss")
  Write-Host ("{0}  marker @ {1} UTC" -f (Get-Date).ToString("HH:mm:ss"), $utc)
  Start-Sleep -Seconds $IntervalSeconds
}
