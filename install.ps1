# Akhub 一键安装 / 升级脚本（Windows）。
#
#   irm https://raw.githubusercontent.com/HsMirage/Akhub/master/install.ps1 | iex
#
# 或先下载再跑（推荐——执行远程脚本前至少看一眼）：
#   irm https://raw.githubusercontent.com/HsMirage/Akhub/master/install.ps1 -OutFile install.ps1
#   notepad install.ps1
#   .\install.ps1 -Version v1.1.0
#
# 参数：
#   -Version <tag>   安装指定版本，默认取最新 Release
#   -Dir <path>      安装目录，默认 $env:LOCALAPPDATA\Programs\Akhub
#   -NoVerify        跳过 sha256 校验（不推荐）
#   -AddToPath       把安装目录加进当前用户的 PATH
#   -DryRun          只打印将要做什么
#
# 解除执行策略限制（只在当前会话内有效，不改系统设置）：
#   Set-ExecutionPolicy -Scope Process -ExecutionPolicy Bypass

[CmdletBinding()]
param(
    [string]$Version = $env:AKHUB_VERSION,
    [string]$Dir = $env:AKHUB_INSTALL_DIR,
    [string]$Repo = $(if ($env:AKHUB_REPO) { $env:AKHUB_REPO } else { 'HsMirage/Akhub' }),
    [switch]$NoVerify,
    [switch]$AddToPath,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
# 老版本 PowerShell 默认不启用 TLS 1.2，连 GitHub 会直接失败。
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

function Write-Info($message) { Write-Host "==> $message" -ForegroundColor Cyan }
function Write-Warn($message) { Write-Host "警告：$message" -ForegroundColor Yellow }
function Fail($message, $code) { Write-Host "install.ps1: $message" -ForegroundColor Red; exit $code }

# ---------------------------------------------------------------- 平台探测
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'arm64' } else { 'x86_64' }
if ($arch -ne 'x86_64') {
    # 目前只发布 windows-x86_64；ARM64 上可以靠 x64 模拟层运行它。
    Write-Warn "当前是 $arch 架构，暂时只有 windows-x86_64 资产；x64 版可以在 ARM64 的模拟层下运行。"
}
$platform = 'windows-x86_64'

if (-not $Dir) {
    $Dir = Join-Path $env:LOCALAPPDATA 'Programs\Akhub'
}

# ---------------------------------------------------------------- 版本解析
if (-not $Version) {
    Write-Info '查询最新版本'
    try {
        $latest = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -UseBasicParsing
        $Version = $latest.tag_name
    } catch {
        Fail "无法确定最新版本（GitHub API 可能限流）：$($_.Exception.Message)。请用 -Version vX.Y.Z 指定。" 3
    }
}
if (-not $Version) { Fail '拿不到版本号' 3 }

# 允许 -Version 1.0.0 与 -Version v1.0.0 两种写法。
$tag = if ($Version.StartsWith('v')) { $Version } else { "v$Version" }

$asset = "akhub-$tag-$platform.zip"
$base = "https://github.com/$Repo/releases/download/$tag"

Write-Info "平台：windows/$arch（$platform）"
Write-Info "版本：$tag"
Write-Info "资产：$asset"

if ($DryRun) {
    Write-Host "将要下载：$base/$asset"
    Write-Host "以及：    $base/checksums.txt"
    Write-Host "安装到：  $(Join-Path $Dir 'akhub.exe')"
    exit 0
}

# ---------------------------------------------------------------- 下载
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("akhub-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

try {
    Write-Info "下载 $asset"
    try {
        Invoke-WebRequest -Uri "$base/$asset" -OutFile (Join-Path $tmp $asset) -UseBasicParsing
    } catch {
        Fail "下载失败：$base/$asset`n$($_.Exception.Message)" 3
    }

    if (-not $NoVerify) {
        Write-Info '校验 sha256'
        try {
            Invoke-WebRequest -Uri "$base/checksums.txt" -OutFile (Join-Path $tmp 'checksums.txt') -UseBasicParsing
            $line = Get-Content (Join-Path $tmp 'checksums.txt') |
                Where-Object { $_ -match ([regex]::Escape($asset) + '\s*$') } |
                Select-Object -First 1
            if (-not $line) { Fail "checksums.txt 里没有 $asset，拒绝安装" 4 }
            $expected = ($line -split '\s+')[0].ToLower()
            $actual = (Get-FileHash -Path (Join-Path $tmp $asset) -Algorithm SHA256).Hash.ToLower()
            if ($actual -ne $expected) {
                Fail "sha256 不匹配，下载可能被篡改或损坏`n  期望：$expected`n  实际：$actual" 4
            }
            Write-Info "校验通过：$actual"
        } catch {
            Write-Warn "拿不到 checksums.txt 或校验过程出错，跳过校验：$($_.Exception.Message)"
        }
    } else {
        Write-Warn '已按 -NoVerify 跳过 sha256 校验'
    }

    # ------------------------------------------------------------ 解包
    Write-Info '解包'
    Expand-Archive -Path (Join-Path $tmp $asset) -DestinationPath $tmp -Force
    $inner = Join-Path $tmp "akhub-$tag-$platform"
    $exe = Join-Path $inner 'akhub.exe'
    if (-not (Test-Path $exe)) { Fail "归档结构与预期不符，找不到 $exe" 4 }

    # ------------------------------------------------------------ 安装
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    $target = Join-Path $Dir 'akhub.exe'

    if (Test-Path $target) {
        $old = try { (& $target --version) } catch { '未知' }
        Write-Info "已安装版本：$old"
        $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
        $backup = "$target.bak-$stamp"
        Copy-Item -Path $target -Destination $backup -Force
        Write-Info "旧版本已备份为 $backup"
    }

    Write-Info "安装到 $target"
    Copy-Item -Path $exe -Destination $target -Force

    $newVersion = try { (& $target --version) } catch { '（无法执行）' }
    Write-Info "完成：$newVersion"

    # ------------------------------------------------------------ PATH
    if ($AddToPath) {
        $userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
        if ($userPath -notlike "*$Dir*") {
            [Environment]::SetEnvironmentVariable('PATH', "$userPath;$Dir", 'User')
            Write-Info "已把 $Dir 加入用户 PATH（新开的终端才会生效）"
        } else {
            Write-Info "$Dir 已经在用户 PATH 里"
        }
    } elseif (($env:PATH -split ';') -notcontains $Dir) {
        Write-Warn "$Dir 不在 PATH 里；想直接敲 akhub 请加 -AddToPath，或手动 cd 过去运行。"
    }

    # ------------------------------------------------------------ 后续
    Write-Host ''
    Write-Host '下一步：' -ForegroundColor Green
    Write-Host '  1. 前台试跑（首次会在数据目录生成主密钥）：'
    Write-Host '       $env:AKHUB_DATA_DIR=''C:\ProgramData\Akhub''; & ''' + $target + ''''
    Write-Host '  2. 打开 http://127.0.0.1:8080/admin 设置管理员密码。'
    Write-Host '  3. 注册为 Windows 服务、反向代理与升级流程见文档：'
    Write-Host '       deploy\README.windows.md（随发行包一起分发）'
    Write-Host ''
    Write-Host '备份提醒：数据目录里的 master.key 一旦丢失，数据库中加密保存的上游 API Key 无法恢复。' -ForegroundColor Yellow
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
