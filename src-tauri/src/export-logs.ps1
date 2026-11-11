# Run by export_logs in main.rs: pack logs, config.json and system info into one zip.
# Paths come in via env vars (AEGIS_LOGS / AEGIS_CONF / AEGIS_ZIP / AEGIS_INFO).
# Keep this file ASCII-only: Windows PowerShell 5.1 reads a BOM-less .ps1 as the ANSI code page.
$ErrorActionPreference = 'Stop'
$t = Join-Path $env:TEMP ('aegis-logs-' + [guid]::NewGuid())
New-Item -ItemType Directory $t | Out-Null
try {
  # Copy first: the current log file is still open for writing by the app
  if (Test-Path $env:AEGIS_LOGS) { Copy-Item (Join-Path $env:AEGIS_LOGS '*') $t }
  if (Test-Path $env:AEGIS_CONF) { Copy-Item $env:AEGIS_CONF $t }

  $q = { param($b) try { & $b } catch { "error: $_" } }
  $os = & $q { Get-CimInstance Win32_OperatingSystem | ForEach-Object { "$($_.Caption) $($_.Version) build $($_.BuildNumber)" } }
  $gpu = & $q { Get-CimInstance Win32_VideoController | ForEach-Object { "$($_.Name) driver $($_.DriverVersion)" } }
  $av = & $q { Get-CimInstance -Namespace root/SecurityCenter2 AntiVirusProduct | ForEach-Object { $_.displayName } }
  $admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
  $run = & $q { (Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name Aegis -ErrorAction Stop).Aegis }
  $approved = & $q { [BitConverter]::ToString((Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run' -Name Aegis -ErrorAction Stop).Aegis) }
  $lnk = Test-Path (Join-Path ([Environment]::GetFolderPath('Startup')) 'Aegis.lnk')

  @(
    $env:AEGIS_INFO
    "OS: $os"
    "GPU: $($gpu -join '; ')"
    "Antivirus: $($av -join '; ')"
    "Admin: $admin"
    "HKCU Run: $run"
    "StartupApproved: $approved"
    "Startup lnk: $lnk"
  ) | Out-File (Join-Path $t 'sysinfo.txt') -Encoding utf8

  Compress-Archive -Path (Join-Path $t '*') -DestinationPath $env:AEGIS_ZIP -Force
} finally {
  Remove-Item -Recurse -Force $t
}
