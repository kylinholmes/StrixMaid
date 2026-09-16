#Requires -Version 5.1
<#
.SYNOPSIS
    停止并注销 StrixMaid 服务、删除二进制。默认【保留】配置与数据。

.DESCRIPTION
    默认行为对应 apt remove 而不是 apt purge：卸载只拿走程序，
    C:\ProgramData\StrixMaid 下的 config.toml 与 SQLite 数据库原样留着。

    这是有意的。数据库里是几个月的指标历史与审计记录，配置里是运维改过的
    监听地址、保留期、提权组。卸载一次就把它们清空，等于把「我先卸掉再装个
    新版本」这种最常见的操作变成一次事故。Linux 侧的 deb 是同一套语义
    （ci.yml 的 deb-smoke 有对应断言：remove 之后 config.toml 必须还在）。

    真的要清干净时加 -Purge。那会删掉整个 DataRoot，【不可撤销】，
    脚本会先让你确认一次。

.PARAMETER InstallDir
    二进制安装目录，默认 %ProgramFiles%\StrixMaid。

.PARAMETER DataRoot
    配置与数据根目录，默认 C:\ProgramData\StrixMaid。只在 -Purge 时用到。

.PARAMETER Purge
    连同配置与数据一并删除。默认关闭。

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File packaging\uninstall.ps1

.EXAMPLE
    # 连配置与历史数据一起删掉
    powershell -NoProfile -ExecutionPolicy Bypass -File packaging\uninstall.ps1 -Purge
#>
[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = 'High')]
param(
    [string] $InstallDir = (Join-Path $env:ProgramFiles 'StrixMaid'),

    [string] $DataRoot = 'C:\ProgramData\StrixMaid',

    [switch] $Purge
)

$ErrorActionPreference = 'Stop'

$ServiceName = 'StrixMaid'

function Assert-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { return }

    # 与 install.ps1 同理：-WhatIf 不写任何东西，只警告不中断。
    if ($WhatIfPreference) {
        Write-Warning '当前不是管理员。-WhatIf 只做干跑，继续；真正卸载前请以管理员身份重新运行。'
        return
    }

    throw @'
需要管理员权限。请以管理员身份打开 PowerShell 或命令提示符再运行本脚本。

原因：注销服务、删除 %ProgramFiles% 下的文件都要求提升后的令牌。
'@
}

Assert-Administrator

$exe = Join-Path $InstallDir 'strixmaid.exe'

# --------------------------------------------------------------------------
# 1. 停服务
# --------------------------------------------------------------------------

$service = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($service) {
    if ($service.Status -ne 'Stopped') {
        if ($PSCmdlet.ShouldProcess($ServiceName, '停止服务')) {
            Write-Host "停止 $ServiceName ..."
            Stop-Service -Name $ServiceName -Force
            $service.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(30))
        }
    }
}
else {
    Write-Host "服务 $ServiceName 未注册，跳过停止与注销。"
}

# --------------------------------------------------------------------------
# 2. 注销服务——必须在删 exe 之前
# --------------------------------------------------------------------------
#
# `strixmaid.exe service uninstall` 是注册的反操作，与 install 同源，
# 因此优先走它。exe 已经不在了（上次卸载删了一半、或有人手工删过）时
# 退回 sc.exe delete：SCM 的注册表项与 exe 是否存在无关，留着一个指向
# 空路径的服务比什么都不做更糟。
if ($service) {
    if ($PSCmdlet.ShouldProcess($ServiceName, '注销 Windows 服务')) {
        $removed = $false
        if (Test-Path -LiteralPath $exe) {
            & $exe service uninstall
            if ($LASTEXITCODE -eq 0) {
                $removed = $true
            }
            else {
                Write-Warning "strixmaid.exe service uninstall 退出码 $LASTEXITCODE，改用 sc.exe delete。"
            }
        }
        else {
            Write-Warning "$exe 不存在，改用 sc.exe delete 注销服务。"
        }

        if (-not $removed) {
            & sc.exe delete $ServiceName | Out-Null
            if ($LASTEXITCODE -ne 0) {
                throw "sc.exe delete $ServiceName 失败（退出码 $LASTEXITCODE）"
            }
        }
    }
}

# --------------------------------------------------------------------------
# 3. 删二进制
# --------------------------------------------------------------------------

if (Test-Path -LiteralPath $InstallDir) {
    foreach ($name in @('strixmaid.exe', 'strixmaid-helper.exe')) {
        $p = Join-Path $InstallDir $name
        if (Test-Path -LiteralPath $p) {
            Remove-Item -LiteralPath $p -Force
        }
    }

    # 目录只在空了之后才删。装在 %ProgramFiles%\StrixMaid 这种专属目录下时
    # 它必然是空的；但 -InstallDir 指到了别处（比如 C:\Tools）时，那里可能
    # 还有别人的东西，连锅端掉就是事故。
    #
    # -WhatIf 下跳过这一段：上面的删除并没有真的发生，此刻数出来的是删除【前】
    # 的状态，报「目录里还有东西」会是一句假话。
    if (-not $WhatIfPreference) {
        $left = @(Get-ChildItem -LiteralPath $InstallDir -Force -ErrorAction SilentlyContinue)
        if ($left.Count -eq 0) {
            Remove-Item -LiteralPath $InstallDir -Force
        }
        else {
            Write-Warning "$InstallDir 下还有其它文件，目录保留。"
        }
    }
}

# --------------------------------------------------------------------------
# 4. 配置与数据
# --------------------------------------------------------------------------

if ($Purge) {
    if (Test-Path -LiteralPath $DataRoot) {
        if ($PSCmdlet.ShouldProcess($DataRoot, '删除配置与全部历史数据（不可撤销）')) {
            Remove-Item -LiteralPath $DataRoot -Recurse -Force
            Write-Host "已删除 $DataRoot" -ForegroundColor Yellow
        }
    }
}
else {
    Write-Host ""
    Write-Host "卸载完成。配置与数据【已保留】：" -ForegroundColor Green
    Write-Host "  $DataRoot"
    Write-Host "重装时会沿用它们；要一并删除请重新运行本脚本并加 -Purge。"
}
