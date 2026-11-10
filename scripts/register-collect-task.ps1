# 注册“采集器”计划任务（与抽干任务完全独立）。
#
# 与 register-drain-tasks.ps1 同一套约定：动作走 wscript 静默启动器（不弹前台控制台），
# MultipleInstances=IgnoreNew，登录后启动 + 每 N 分钟重复。
#
# 用法（管理员 PowerShell）：
#   .\register-collect-task.ps1 `
#     -Binary "D:\recollect\.local\bin\recollect-hub.exe" `
#     -Config "D:\recollect\.local\collect.toml" `
#     -DataDir "D:\recollect\.local\node-data" `
#     -Log "D:\recollect\.local\node-data\logs\collect.log"
#
# 只注册采集；不碰 recollect-drain-trigger/full。
param(
    [Parameter(Mandatory = $true)][string]$Binary,
    [Parameter(Mandatory = $true)][string]$Config,
    [Parameter(Mandatory = $true)][string]$DataDir,
    [Parameter(Mandatory = $true)][string]$Log,
    [string]$TaskName = "recollect-collect",
    [int]$IntervalMinutes = 30
)

$ErrorActionPreference = "Stop"

$wscript = "C:\Windows\System32\wscript.exe"
$binDir = Split-Path -Parent $Config
$vbsPath = Join-Path $binDir "run-collect-hidden.vbs"

# 启动器：隐藏窗口；等待退出，保证 IgnoreNew 期间实例互斥仍然有效。
$vbs = @"
' collector launcher (generated; do not edit by hand)
Set sh = CreateObject("WScript.Shell")
cmd = "cmd /C set RECOLLECT_LOG=$Log&& ""$Binary"" collect --config ""$Config"" --data-dir ""$DataDir"" --json >> ""$Log"" 2>&1"
sh.Run cmd, 0, True
WScript.Quit 0
"@
Set-Content -Path $vbsPath -Value $vbs -Encoding ASCII

$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -StartWhenAvailable -MultipleInstances IgnoreNew
$action = New-ScheduledTaskAction -Execute $wscript -Argument ('"' + $vbsPath + '"')
$trigger = New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(2) `
    -RepetitionInterval (New-TimeSpan -Minutes $IntervalMinutes) `
    -RepetitionDuration (New-TimeSpan -Days 3650)

Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger `
    -Settings $settings -Description "recollect session collector (zero-model, edge-side)" -Force | Out-Null

Get-ScheduledTask -TaskName $TaskName | Select-Object TaskName, State | Format-Table -AutoSize
