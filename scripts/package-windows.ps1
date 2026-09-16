#Requires -Version 5.1
<#
.SYNOPSIS
    组装 Windows 发布 zip，与 scripts/package.sh 的 tar.gz 同构。

.DESCRIPTION
    产物布局刻意与 Linux 侧一致（roadmap/06 §3.5），换平台的人不必重新认路：

        strixmaid-<版本>-x86_64-windows/
        ├── strixmaid.exe
        ├── strixmaid-helper.exe
        ├── config.example.toml     由 strixmaid.exe config example 现场生成
        ├── README.md               安装与卸载说明（packaging/windows/README.md）
        ├── LICENSE
        └── packaging/
            ├── install.ps1
            └── uninstall.ps1

    发布物里【不含】strixmaid-agent.exe。Windows 侧的服务宿主（`service`
    子命令族）只做在 strixmaid-server 里，agent 没有被 SCM 托管的入口，
    装进来也只能手工前台运行——那不是可交付的形态。补上服务宿主之后再说。

    为什么 config.example.toml 也进包：它是给人在安装【之前】读的，
    好知道装完会得到什么。install.ps1 不会用这一份，而是用装好的
    strixmaid.exe 现场再生成一次（见该脚本的说明），避免包里的快照过期。

.PARAMETER Configuration
    release（默认）或 debug。debug 只用于本机验证脚本本身，不要拿去分发。

.PARAMETER OutDir
    zip 的输出目录，默认为仓库根。

.EXAMPLE
    pwsh -NoProfile -File scripts\package-windows.ps1
#>
[CmdletBinding()]
param(
    [ValidateSet('release', 'debug')]
    [string] $Configuration = 'release',

    [string] $OutDir
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSCommandPath)
if (-not $OutDir) { $OutDir = $repoRoot }

# 版本号取自 workspace 的 Cargo.toml，与 package.sh / ci.yml 的取法一致。
$versionLine = Select-String -Path (Join-Path $repoRoot 'Cargo.toml') -Pattern '^version' |
    Select-Object -First 1
if (-not $versionLine) { throw 'Cargo.toml 里找不到 version' }
$version = [regex]::Match($versionLine.Line, '"([^"]+)"').Groups[1].Value
if (-not $version) { throw "无法从 `"$($versionLine.Line)`" 解析版本号" }

$target = 'x86_64-pc-windows-msvc'
$name = "strixmaid-$version-x86_64-windows"

Write-Host "构建 $Configuration（$target）..." -ForegroundColor Cyan
Push-Location $repoRoot
try {
    $cargoArgs = @('build', '--target', $target, '-p', 'strixmaid-server', '-p', 'strixmaid-helper')
    if ($Configuration -eq 'release') { $cargoArgs += '--release' }
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build 失败（退出码 $LASTEXITCODE）" }
}
finally {
    Pop-Location
}

# cargo 的 target 目录可以被 CARGO_TARGET_DIR 或 .cargo/config.toml 改掉，
# 因此问 cargo metadata 而不是假定 <repo>/target——本机就有一处改动。
$metadata = & cargo metadata --no-deps --format-version 1 --manifest-path (Join-Path $repoRoot 'Cargo.toml') |
    ConvertFrom-Json
$binDir = Join-Path (Join-Path $metadata.target_directory $target) $Configuration

$serverExe = Join-Path $binDir 'strixmaid.exe'
$helperExe = Join-Path $binDir 'strixmaid-helper.exe'
foreach ($exe in @($serverExe, $helperExe)) {
    if (-not (Test-Path -LiteralPath $exe)) { throw "构建产物缺失：$exe" }
}

$stage = Join-Path ([System.IO.Path]::GetTempPath()) ("strixmaid-pkg-" + [guid]::NewGuid().ToString('N'))
$root = Join-Path $stage $name
$null = New-Item -ItemType Directory -Path (Join-Path $root 'packaging') -Force

Copy-Item -LiteralPath $serverExe -Destination $root
Copy-Item -LiteralPath $helperExe -Destination $root
Copy-Item -LiteralPath (Join-Path $repoRoot 'LICENSE') -Destination $root
Copy-Item -LiteralPath (Join-Path $repoRoot 'packaging\windows\README.md') -Destination $root
Copy-Item -LiteralPath (Join-Path $repoRoot 'packaging\windows\install.ps1') -Destination (Join-Path $root 'packaging')
Copy-Item -LiteralPath (Join-Path $repoRoot 'packaging\windows\uninstall.ps1') -Destination (Join-Path $root 'packaging')

# 示例配置由【刚构建出来的】exe 生成，不是仓库里的某份快照：
# Config::example_toml() 会按平台替换路径与提权组，手抄一份必然会漂。
#
# 用 Start-Process 重定向而不是 `&`：PowerShell 5.1 捕获子进程输出时按
# [Console]::OutputEncoding 解码，而那在中文 Windows 上默认是 GBK（936）。
# 示例配置里全是中文注释，走一遍解码再写回去就成了乱码。重定向写的是原始字节。
$examplePath = Join-Path $root 'config.example.toml'
$proc = Start-Process -FilePath $serverExe -ArgumentList 'config', 'example' `
    -NoNewWindow -Wait -PassThru -RedirectStandardOutput $examplePath
if ($proc.ExitCode -ne 0) { throw "strixmaid config example 失败（退出码 $($proc.ExitCode)）" }

$zip = Join-Path $OutDir "$name.zip"
if (Test-Path -LiteralPath $zip) { Remove-Item -LiteralPath $zip -Force }
Compress-Archive -Path $root -DestinationPath $zip -CompressionLevel Optimal
Remove-Item -LiteralPath $stage -Recurse -Force

Write-Host ""
Write-Host "已生成 $zip" -ForegroundColor Green
Get-Item -LiteralPath $serverExe, $helperExe, $zip |
    Select-Object @{ n = '大小(字节)'; e = { $_.Length } }, FullName |
    Format-Table -AutoSize
