#Requires -Version 5.1
<#
.SYNOPSIS
    把 StrixMaid 装到本机并注册成 Windows 服务。

.DESCRIPTION
    对应 Linux 侧的 packaging/install.sh（roadmap/06 §3.4），做同样的五件事：
    放二进制、建目录、写默认配置、注册服务、给出启动提示。幂等——重复执行安全，
    已存在的配置不会被覆盖。

    与 install.sh 的两处实质差别：

    * 没有 pam.d。Windows 的认证走 LogonUserW，不读 /etc/pam.d
      （见 crates/strixmaid-core/src/config.rs 里 DEFAULT_PAM_SERVICE 的说明）。
    * 服务注册【不】在这里用 New-Service 做，而是调用
      `strixmaid.exe service install`。理由：那条子命令还会设置恢复策略
      （SERVICE_CONFIG_FAILURE_ACTIONS）与服务描述，New-Service 给不了；
      把注册逻辑放在二进制里，命令行装与脚本装得到的是同一个服务。

    脚本本身兼容 Windows 自带的 PowerShell 5.1，不需要装 PowerShell 7。

.PARAMETER InstallDir
    二进制安装目录，默认 %ProgramFiles%\StrixMaid。

.PARAMETER DataRoot
    配置与数据的根目录，默认 C:\ProgramData\StrixMaid。

    这个默认值是【写死】的字面量，不是 %ProgramData% 展开的结果——
    strixmaid 内置的默认配置路径同样是编译期写死的字面量
    （crates/strixmaid-core/src/config.rs 的 DEFAULT_CONFIG_PATH），两边必须一致。
    改动本参数时务必同时用 --config / STRIXMAID_CONFIG 告诉服务新路径，
    否则服务会去读 C:\ProgramData\StrixMaid\config.toml 而不是你装的那份。

.PARAMETER Account
    服务登录账户，默认 LocalSystem。只接受三个内置的、无口令的服务账户。

.PARAMETER StartService
    注册完成后立即启动服务。默认不启动，与 install.sh「不自动 enable」一致。

.EXAMPLE
    # 先干跑一遍看会动什么
    powershell -NoProfile -ExecutionPolicy Bypass -File packaging\install.ps1 -WhatIf

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File packaging\install.ps1 -StartService
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string] $InstallDir = (Join-Path $env:ProgramFiles 'StrixMaid'),

    [string] $DataRoot = 'C:\ProgramData\StrixMaid',

    [ValidateSet('LocalSystem', 'NT AUTHORITY\LocalService', 'NT AUTHORITY\NetworkService')]
    [string] $Account = 'LocalSystem',

    [switch] $StartService
)

$ErrorActionPreference = 'Stop'

# 服务名。与 crates/strixmaid-server/src/service.rs 的 SERVICE_NAME 同一个值；
# uninstall.ps1 在 exe 已被删掉时要用它兜底。
$ServiceName = 'StrixMaid'

# --------------------------------------------------------------------------
# 工具函数
# --------------------------------------------------------------------------

function Assert-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { return }

    # -WhatIf 是干跑，一个字节都不写。此时只警告不中断——否则「先看看它会动什么」
    # 这件事本身也得先提权，那就失去意义了。
    if ($WhatIfPreference) {
        Write-Warning '当前不是管理员。-WhatIf 只做干跑，继续；真正安装前请以管理员身份重新运行。'
        return
    }

    throw @'
需要管理员权限。请以管理员身份打开 PowerShell 或命令提示符再运行本脚本
（开始菜单里右键「Windows PowerShell」→「以管理员身份运行」）。

原因：要写入 %ProgramFiles%、改 %ProgramData% 下的目录 ACL、并向服务控制管理器
注册服务，这三件事都要求提升后的令牌。
'@
}

function Invoke-Native {
    param(
        [Parameter(Mandatory = $true)][string] $FilePath,
        [string[]] $ArgumentList = @(),
        [Parameter(Mandatory = $true)][string] $What
    )
    & $FilePath @ArgumentList
    if ($LASTEXITCODE -ne 0) {
        throw "$What 失败：$FilePath 退出码 $LASTEXITCODE"
    }
}

# 只给 SYSTEM 与 Administrators 完全控制，可选再给 Users 只读 + 执行。
#
# 为什么必须显式设置而不是靠继承：%ProgramData% 的默认 ACL 里有一条给
# Users 的「创建文件 / 写入数据」+「创建文件夹 / 追加数据」，且带容器继承——
# 在它下面新建的目录会原样继承过来。也就是说什么都不做的话，任何登录用户
# 都能往 C:\ProgramData\StrixMaid 里丢文件。
#
# 那是提权面，不是洁癖：服务以 LocalSystem 读这里的 config.toml，而配置里
# 的 helper_path 是一条会被 CreateProcess 执行的路径。普通用户改得动配置，
# 就等于能让 SYSTEM 执行任意程序。
#
# 继承标志的含义：(OI) 对象继承——本目录下的【文件】继承这条 ACE；
# (CI) 容器继承——本目录下的【子目录】继承。两个一起给，才覆盖整棵子树。
# /inheritance:r 断掉从父目录继承来的 ACE，放在 /grant 之后执行，
# 免得中间出现一个谁都进不去的空 DACL。
#
# 全部用 SID 而不是组名：内建组的显示名是【本地化】的（德文 Windows 上
# Administrators 叫 Administratoren），按名字授权在非英文系统上会直接报错。
# 这与 platform/windows/token.rs 给内建组补英文规范名是同一个理由。
function Set-StrixMaidAcl {
    param(
        [Parameter(Mandatory = $true)][string] $Path,
        [switch] $AllowUsersRead
    )

    $grants = @(
        '*S-1-5-18:(OI)(CI)F'      # NT AUTHORITY\SYSTEM
        '*S-1-5-32-544:(OI)(CI)F'  # BUILTIN\Administrators
    )
    if ($AllowUsersRead) {
        $grants += '*S-1-5-32-545:(OI)(CI)RX'  # BUILTIN\Users
    }

    Invoke-Native -FilePath 'icacls.exe' -What "设置 $Path 的 ACL" `
        -ArgumentList (@($Path, '/grant:r') + $grants + @('/Q'))
    Invoke-Native -FilePath 'icacls.exe' -What "断开 $Path 的 ACL 继承" `
        -ArgumentList @($Path, '/inheritance:r', '/Q')
}

# --------------------------------------------------------------------------
# 0. 前置检查
# --------------------------------------------------------------------------

Assert-Administrator

# 脚本位于发布包的 packaging\ 下，二进制在它的上一级。
$distRoot = Split-Path -Parent (Split-Path -Parent $PSCommandPath)
$srcServer = Join-Path $distRoot 'strixmaid.exe'
$srcHelper = Join-Path $distRoot 'strixmaid-helper.exe'

foreach ($f in @($srcServer, $srcHelper)) {
    if (-not (Test-Path -LiteralPath $f)) {
        throw "发布包不完整，缺 $f。请在解压后的目录里运行 packaging\install.ps1。"
    }
}

$dstServer = Join-Path $InstallDir 'strixmaid.exe'
$dstHelper = Join-Path $InstallDir 'strixmaid-helper.exe'
$configPath = Join-Path $DataRoot 'config.toml'
$dataDir = Join-Path $DataRoot 'data'
$runDir = Join-Path $DataRoot 'run'

# --------------------------------------------------------------------------
# 1. 已装过就先停下来——运行中的 exe 在 Windows 上是【锁住的】，覆盖会失败
# --------------------------------------------------------------------------

$existing = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($existing -and $existing.Status -ne 'Stopped') {
    if ($PSCmdlet.ShouldProcess($ServiceName, '停止服务以便覆盖二进制')) {
        Write-Host "停止正在运行的服务 $ServiceName ..."
        Stop-Service -Name $ServiceName -Force
        $existing.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(30))
    }
}

# --------------------------------------------------------------------------
# 2. 目录与二进制
# --------------------------------------------------------------------------

foreach ($d in @($InstallDir, $DataRoot, $dataDir, $runDir)) {
    if (-not (Test-Path -LiteralPath $d)) {
        $null = New-Item -ItemType Directory -Path $d -Force
    }
}

Copy-Item -LiteralPath $srcServer -Destination $dstServer -Force
Copy-Item -LiteralPath $srcHelper -Destination $dstHelper -Force

# --------------------------------------------------------------------------
# 3. ACL
# --------------------------------------------------------------------------
#
# 安装目录给 Users 只读 + 执行，这一条是【必需】的而不是顺手：worker 以
# 登录用户的身份运行，而 worker 就是 strixmaid.exe 的一个子命令
# （design.md §2.1：worker 不是独立二进制）。Users 读不到这个文件，
# 登录之后什么都干不了。写权限当然不给——服务以 LocalSystem 执行它。
#
# 数据目录一条都不给 Users：worker 不读配置文件（见 cli.rs 里 worker 子命令
# 的说明），SQLite 只有主进程碰；审计与指标历史也不该让普通用户直接翻。
if ($PSCmdlet.ShouldProcess($InstallDir, '设置目录 ACL')) {
    Set-StrixMaidAcl -Path $InstallDir -AllowUsersRead
}
if ($PSCmdlet.ShouldProcess($DataRoot, '设置目录 ACL')) {
    Set-StrixMaidAcl -Path $DataRoot
}

# --------------------------------------------------------------------------
# 4. 默认配置（已存在时不覆盖）
# --------------------------------------------------------------------------
#
# 由刚装好的 exe 现场生成，不带一份硬编的副本：Config::example_toml() 会按
# 平台替换路径、提权组与平台提示行，手抄的那份迟早与代码里的默认值对不上，
# 而「示例配置的每一项都等于内置默认值」是有单元测试保证的性质。
#
# 两处编码细节，都踩过才知道：
#
# * 用 Start-Process 重定向而不是 `& exe config example`。PowerShell 5.1
#   捕获子进程输出时按 [Console]::OutputEncoding 解码，中文 Windows 上那是
#   GBK（936），示例配置里的中文注释会整片变成乱码。重定向落的是原始字节。
# * 回写时用不带 BOM 的 UTF-8。Set-Content -Encoding UTF8 在 5.1 上【会】写
#   BOM，而 TOML 规范不允许文件以 BOM 开头，解析器会直接报错——配置文件
#   看着好好的却启动不了，极难查。
if (Test-Path -LiteralPath $configPath) {
    Write-Host "保留已存在的 $configPath"
}
elseif ($PSCmdlet.ShouldProcess($configPath, '生成默认配置')) {
    $proc = Start-Process -FilePath $dstServer -ArgumentList 'config', 'example' `
        -NoNewWindow -Wait -PassThru -RedirectStandardOutput $configPath
    if ($proc.ExitCode -ne 0) {
        throw "strixmaid config example 失败（退出码 $($proc.ExitCode)）"
    }

    # 把 helper_path 固定成绝对路径。
    #
    # 示例里的默认值是裸名 'strixmaid-helper'，那要靠 PATH 查找
    # （capability::find_executable），而服务继承的是【系统】PATH，
    # %ProgramFiles%\StrixMaid 不在里面。装完却登录不了、日志只说
    # 「helper 不可用」，原因就在这。改 PATH 是全局副作用，写绝对路径
    # 只影响本实例，所以选后者。
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    $text = [System.IO.File]::ReadAllText($configPath, $utf8NoBom)
    $escaped = $dstHelper -replace "'", "''"
    $patched = [regex]::Replace(
        $text,
        "(?m)^helper_path\s*=.*$",
        "helper_path = '$escaped'")
    [System.IO.File]::WriteAllText($configPath, $patched, $utf8NoBom)

    Write-Host "已生成 $configPath"
}

if ($PSCmdlet.ShouldProcess($configPath, '校验配置')) {
    # --check-config 校验的是【合并后】的最终结果（roadmap/06 的决策记录），
    # 配置文件不存在时校验的就是内置默认值，同样有意义。
    Invoke-Native -FilePath $dstServer -What '配置校验' `
        -ArgumentList @('--config', $configPath, '--check-config')
}

# --------------------------------------------------------------------------
# 5. 注册服务
# --------------------------------------------------------------------------
#
# 交给 `strixmaid.exe service install` 而不是 New-Service：
# 除了建服务，它还要写恢复策略与服务描述，并把 ImagePath 写成
# 「绝对路径 + service run」——服务由 services.exe 拉起，工作目录是
# %SystemRoot%\system32，相对路径解析不到。这些 New-Service 都做不到。
#
# 该子命令本身是幂等的，重复安装（升级场景）不需要先卸载。
if ($PSCmdlet.ShouldProcess($ServiceName, '注册 Windows 服务')) {
    $svcArgs = @('service', 'install', '--exe', $dstServer, '--account', $Account)
    if ($StartService) { $svcArgs += '--start' }
    Invoke-Native -FilePath $dstServer -ArgumentList $svcArgs -What '注册服务'
}

# --------------------------------------------------------------------------
# 6. 提示
# --------------------------------------------------------------------------

$listen = '127.0.0.1:9700'
if (Test-Path -LiteralPath $configPath) {
    $line = Select-String -Path $configPath -Pattern '^\s*listen\s*=' | Select-Object -First 1
    if ($line) {
        $m = [regex]::Match($line.Line, '["'']([^"'']+)["'']')
        if ($m.Success) { $listen = $m.Groups[1].Value }
    }
}

Write-Host ""
Write-Host "安装完成。" -ForegroundColor Green
Write-Host "  二进制    $InstallDir"
Write-Host "  配置      $configPath"
Write-Host "  数据      $dataDir"
Write-Host "  监听地址  $listen"
Write-Host ""
Write-Host "启动：    $dstServer service start"
Write-Host "查看状态：$dstServer service status"
Write-Host "卸载：    packaging\uninstall.ps1（默认保留配置与数据，加 -Purge 才删）"
Write-Host ""
Write-Host "默认只监听 127.0.0.1；对外访问请在前面配置反向代理（TLS 在反代终结）。"
