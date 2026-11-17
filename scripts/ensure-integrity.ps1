# ============================================================
#  ensure-integrity.ps1 — 确保项目目录的完整性标签是 Medium
#
#  为什么需要它：
#    某些沙箱工具（已知：Codex 的 CodexSandboxService.OpenAI.Codex）
#    会给工作目录打上 `Mandatory Label\Low Mandatory Level`。
#    Windows 的规则是「进程完整性 = min(父进程, 可执行文件)」，
#    所以 exe 一旦带上 Low 标签，**任何人启动它都是 Low 完整性**，
#    也就写不进 Medium 级别的 %LOCALAPPDATA%（日志、封面缓存、
#    VSR 转码分片、worker 下载全部失败），表现为：
#        播放一直缓冲 / 封面全空 / 日志不更新
#    而这个故障在 UI 上完全看不出原因，极易误判成代码 bug。
#
#  本脚本做两件事（均不需要管理员权限，目录属主即可）：
#    1) 检查项目根标签，是 Low 就恢复成 Medium 并开启继承；
#    2) 单独确认 exe 也是 Medium（防止它曾被显式设成 Low）。
#
#  退出码：0 = 正常或已修复；1 = 修复失败（需要人工介入）。
# ============================================================

$ErrorActionPreference = 'Continue'

# 让中文输出在 chcp 65001 的 cmd 窗口里正常显示
try { [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 } catch { }

$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$exe  = Join-Path $root 'src-tauri\target\debug\ttv-short-drama.exe'
$ffmpeg = Join-Path $root 'src-tauri\resources\mpv\ffmpeg.exe'

function Ensure-TtvPlayerStaged {
    param([string]$RootDir)
    $crateDir = Join-Path $RootDir 'src-tauri\ttv-player'
    $manifest = Join-Path $crateDir 'Cargo.toml'
    if (-not (Test-Path -LiteralPath $manifest -PathType Leaf)) {
        Write-Host "[integrity] missing ttv-player manifest: $manifest"
        return $false
    }

    $binSrc = Join-Path $crateDir 'target\release\ttv-player.exe'
    $dllSrc = Join-Path $RootDir 'src-tauri\native-player\libmpv-2.dll'
    if (-not (Test-Path -LiteralPath $dllSrc -PathType Leaf)) {
        if ($env:TTV_REQUIRE_RUNTIME_BINS -eq '1') {
            Write-Host "[integrity] missing libmpv-2.dll asset: $dllSrc"
            return $false
        }
        Write-Host "[integrity] libmpv-2.dll not present in source-only checkout, skipping ttv-player staging."
        return $true
    }

    $needsBuild = -not (Test-Path -LiteralPath $binSrc -PathType Leaf)
    if (-not $needsBuild) {
        $binTime = (Get-Item -LiteralPath $binSrc).LastWriteTimeUtc
        $watchPaths = @(
            $manifest,
            (Join-Path $crateDir 'Cargo.lock'),
            (Join-Path $crateDir 'build.rs')
        )
        $watchDirs = @('src', 'assets', 'shaders') | ForEach-Object { Join-Path $crateDir $_ } | Where-Object { Test-Path -LiteralPath $_ }
        $newerInputs = @()
        foreach ($wp in $watchPaths) {
            if ((Test-Path -LiteralPath $wp -PathType Leaf) -and ((Get-Item -LiteralPath $wp).LastWriteTimeUtc -gt $binTime)) {
                $newerInputs += $wp
            }
        }
        foreach ($wd in $watchDirs) {
            $newerInputs += Get-ChildItem -LiteralPath $wd -Recurse -File | Where-Object { $_.LastWriteTimeUtc -gt $binTime }
        }
        if ($newerInputs.Count -gt 0) {
            $needsBuild = $true
        }
    }

    if ($needsBuild) {
        $cargoExe = $null
        if ($env:TTV_CARGO_BIN -and (Test-Path -LiteralPath $env:TTV_CARGO_BIN -PathType Leaf)) {
            $cargoExe = $env:TTV_CARGO_BIN
        } elseif ($cmd = Get-Command cargo -ErrorAction SilentlyContinue) {
            $cargoExe = $cmd.Source
        } elseif (Test-Path -LiteralPath (Join-Path $HOME '.cargo\bin\cargo.exe') -PathType Leaf) {
            $cargoExe = Join-Path $HOME '.cargo\bin\cargo.exe'
        }
        if (-not $cargoExe) {
            Write-Host '[integrity] cargo not found in PATH or $HOME\.cargo\bin.'
            return $false
        }
        $tmpDir = Join-Path $RootDir '.tmp'
        if (-not (Test-Path -LiteralPath $tmpDir)) {
            New-Item -ItemType Directory -Force -Path $tmpDir | Out-Null
        }
        $prevTemp = $env:TEMP
        $prevTmp = $env:TMP
        $env:TEMP = $tmpDir
        $env:TMP = $tmpDir
        try {
            Write-Host '[integrity] building release ttv-player.exe ...'
            & $cargoExe build --release --offline --manifest-path $manifest
            if (-not $? -or $LASTEXITCODE -ne 0) {
                Write-Host '[integrity] offline build failed, retrying with online cargo ...'
                & $cargoExe build --release --manifest-path $manifest
                if (-not $? -or $LASTEXITCODE -ne 0) {
                    Write-Host '[integrity] cargo build ttv-player failed.'
                    return $false
                }
            }
        } finally {
            $env:TEMP = $prevTemp
            $env:TMP = $prevTmp
        }
    }

    $stageDir = Join-Path $RootDir 'src-tauri\resources\ttv-player'
    New-Item -ItemType Directory -Force -Path $stageDir | Out-Null
    foreach ($pair in @(
        @{ Src = $binSrc; Dst = (Join-Path $stageDir 'ttv-player.exe') },
        @{ Src = $dllSrc; Dst = (Join-Path $stageDir 'libmpv-2.dll') }
    )) {
        $copyNeeded = -not (Test-Path -LiteralPath $pair.Dst -PathType Leaf)
        if (-not $copyNeeded) {
            $sItem = Get-Item -LiteralPath $pair.Src
            $dItem = Get-Item -LiteralPath $pair.Dst
            if ($sItem.LastWriteTimeUtc -gt $dItem.LastWriteTimeUtc -or $sItem.Length -ne $dItem.Length) {
                $copyNeeded = $true
            }
        }
        if ($copyNeeded) {
            try {
                Copy-Item -LiteralPath $pair.Src -Destination $pair.Dst -Force -ErrorAction Stop
                Write-Host "[integrity] staged -> $($pair.Dst)"
            } catch {
                if (Test-Path -LiteralPath $pair.Dst -PathType Leaf) {
                    Write-Host "[integrity] warning: $($pair.Dst) is in use, keeping existing staged file: $($_.Exception.Message)"
                } else {
                    Write-Host "[integrity] failed to stage $($pair.Dst): $($_.Exception.Message)"
                    return $false
                }
            }
        }
    }

    $stagedExe = Join-Path $stageDir 'ttv-player.exe'
    $stagedDll = Join-Path $stageDir 'libmpv-2.dll'
    if (-not (Test-Path -LiteralPath $stagedExe -PathType Leaf) -or -not (Test-Path -LiteralPath $stagedDll -PathType Leaf)) {
        Write-Host '[integrity] staged ttv-player runtime assets incomplete.'
        return $false
    }
    Write-Host '[integrity] ttv-player staged assets ready.'
    return $true
}

function Get-IntegrityLabel {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) { return $null }
    $line = icacls $Path 2>&1 | Select-String -Pattern 'Mandatory Label' | Select-Object -First 1
    if (-not $line) { return $null }
    if ($line -match '(Low|Medium|High|System)\s+Mandatory\s+Level') { return $matches[1] }
    return 'Unknown'
}

function Set-Medium {
    param([string]$Path, [switch]$Recurse)
    if ($Recurse) {
        icacls $Path /setintegritylevel "(OI)(CI)M" 2>&1 | Out-Null
    } else {
        icacls $Path /setintegritylevel M 2>&1 | Out-Null
    }
}

function Test-FfmpegAsset {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        Write-Host "[integrity] missing ffmpeg asset: $Path"
        return $false
    }

    $stream = $null
    try {
        $stream = [System.IO.File]::OpenRead($Path)
        $buffer = New-Object byte[] 128
        $read = $stream.Read($buffer, 0, $buffer.Length)
    } catch {
        Write-Host "[integrity] cannot read ffmpeg asset: $($_.Exception.Message)"
        return $false
    } finally {
        if ($stream) { $stream.Dispose() }
    }

    $header = [System.Text.Encoding]::ASCII.GetString($buffer, 0, $read)
    if ($header.StartsWith('version https://git-lfs.github.com/spec/v1')) {
        Write-Host '[integrity] ffmpeg.exe is still a Git LFS pointer; run git lfs checkout.'
        return $false
    }
    if ($read -lt 2 -or $buffer[0] -ne 0x4D -or $buffer[1] -ne 0x5A) {
        Write-Host '[integrity] ffmpeg.exe is not a Windows executable.'
        return $false
    }
    if ((Get-Item -LiteralPath $Path).Length -lt 1MB) {
        Write-Host '[integrity] ffmpeg.exe is unexpectedly small.'
        return $false
    }

    & $Path -hide_banner -version 1>$null 2>$null
    if (-not $? -or $LASTEXITCODE -ne 0) {
        Write-Host '[integrity] ffmpeg.exe failed to start.'
        return $false
    }
    Write-Host '[integrity] ffmpeg asset is a runnable Windows executable.'
    return $true
}

$fixed = $false
$failed = $false

# ---- 1. 项目根 ----
$rootLabel = Get-IntegrityLabel $root
if ($rootLabel -eq $null) {
    Write-Host "[integrity] 项目根没有显式标签（等效 Medium），无需处理。"
} elseif ($rootLabel -ne 'Medium') {
    Write-Host "[integrity] 检测到项目根标签 = $rootLabel，正在恢复为 Medium ..."
    Set-Medium $root -Recurse
    $after = Get-IntegrityLabel $root
    if ($after -eq 'Medium') {
        Write-Host "[integrity] 已恢复：项目根 -> Medium（子项通过继承生效）"
        $fixed = $true
    } else {
        Write-Host "[integrity] 恢复失败：当前仍为 $after"
        $failed = $true
    }
} else {
    Write-Host "[integrity] 项目根 = Medium，正常。"
}

# ---- 2. exe（防显式 Low）----
if (Test-Path -LiteralPath $exe) {
    $exeLabel = Get-IntegrityLabel $exe
    if ($exeLabel -eq 'Low') {
        Write-Host "[integrity] exe 标签 = Low（显式设置），单独恢复 ..."
        Set-Medium $exe
        $exeAfter = Get-IntegrityLabel $exe
        if ($exeAfter -eq 'Medium') {
            Write-Host "[integrity] 已恢复：exe -> Medium"
            $fixed = $true
        } else {
            Write-Host "[integrity] exe 恢复失败：当前 $exeAfter"
            $failed = $true
        }
    } else {
        Write-Host "[integrity] exe = $exeLabel，正常。"
    }
} else {
    Write-Host "[integrity] 尚未构建 exe，跳过。"
}

# ---- 3. bundled runtime assets ----
if (Test-Path -LiteralPath $ffmpeg -PathType Leaf) {
    if (-not (Test-FfmpegAsset $ffmpeg)) { $failed = $true }
} elseif ($env:TTV_REQUIRE_RUNTIME_BINS -eq '1') {
    Write-Host "[integrity] missing ffmpeg asset: $ffmpeg"
    $failed = $true
} else {
    Write-Host "[integrity] ffmpeg.exe not present in source-only checkout, skipping."
}
if (-not (Ensure-TtvPlayerStaged $root)) { $failed = $true }

if ($fixed) { Write-Host "[integrity] 本次已修复。" }
if ($failed) { exit 1 }
exit 0
