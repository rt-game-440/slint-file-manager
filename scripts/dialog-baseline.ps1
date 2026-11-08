# Issue #23：用测试宿主（tools/dialog-host）驱动 Windows 自带文件选择对话框，记录基准并逐字段比对。
# 运行在 Windows PowerShell 5.1：powershell -File scripts/dialog-baseline.ps1 -Mode <模式> [...]
#
# 模式：
#   list      列出全部用例（方式×场景×驱动动作），不弹框。
#   record    构建（除非 -SkipBuild）后跑完全部用例，写入基准 tests/dialog-baseline/<系统>.json（入库）。
#   compare   跑一遍用例并与基准逐字段比对（默认）。退出码 0 一致、1 有差异、2 无法判断（如该系统没有基准＝未覆盖）。
#   diff      不弹框：比较 -Actual 指定的结果文件与基准（用于“接管后”的结果；结构与 record 相同）。
#   selftest  不弹框：在基准副本上人为制造字段差异，证明比对入口能逐字段检出；退出码 0 全部检出。
#
# 自动驱动只操作本测试宿主进程弹出的对话框（按进程号枚举窗口），不碰其他窗口，不截图桌面。
# 夹具在 %TEMP%\sfm-dialog-host\... 下每次重建；基准里的夹具路径归一化为 {FIXTURE}，不含用户名。
param(
    [ValidateSet('list', 'record', 'compare', 'diff', 'selftest')][string]$Mode = 'compare',
    [string[]]$Bits = @('x64', 'x86'),
    [string]$Filter = '',
    [string]$Actual = '',
    [string]$Baseline = '',
    [string]$Out = '',
    [string]$WorkDir = '',
    [switch]$SkipBuild,
    # 人为扰动：让驱动在 ifd-open-single 里选另一个文件，用来演示 compare 能在真实运行里检出差异
    [switch]$Perturb,
    [int]$CaseTimeoutSec = 40
)
$ErrorActionPreference = 'Stop'
# powershell -File 下 "-Bits x64,x86" 会得到一个字符串，统一拆开
$Bits = @($Bits | ForEach-Object { $_ -split ',' } | Where-Object { $_ })
$Root =(Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$HostDir = Join-Path $Root 'tools\dialog-host'
$BaselineDir = Join-Path $Root 'tests\dialog-baseline'
$Utf8 = New-Object System.Text.UTF8Encoding($false)

# ---------------------------------------------------------------- 用例表
# 每个用例：Id、Api、Scenario、Args（宿主选项）、Type（输入到文件名框的文字）、FilterSel（驱动切换到第几个过滤器）、
# Action（ok／cancel）、Confirm（出现覆盖确认时 yes／no）、Desc。
# 占位符：{DOCS}＝夹具 docs 目录，{FIXTURE}＝夹具根目录。
$F3 = @('--filter=文本文件|*.txt;*.md', '--filter=图片|*.png;*.jpg', '--filter=所有文件|*.*')
$Cases = New-Object System.Collections.ArrayList
function Add-Case($id, $api, $scenario, $desc, $caseArgs = @(), $type = $null, $filterSel = 0, $action = 'ok', $confirm = 'yes') {
    [void]$script:Cases.Add([pscustomobject]@{
            Id = $id; Api = $api; Scenario = $scenario; Desc = $desc; Args = $caseArgs; Type = $type
            FilterSel = $filterSel; Action = $action; Confirm = $confirm
        })
}
$docs = '--init-dir={DOCS}'
$subdir = '--init-dir={DOCS}\sub'
foreach ($api in 'ifd', 'gofn') {
    $p = $api
    Add-Case "$p-open-single" $api 'open-single' '打开单个：在初始目录输入文件名' (@($docs) + $F3) 'alpha.txt'
    Add-Case "$p-open-initdir-sub" $api 'open-single' '初始目录是子目录，返回子目录里的文件' (@($subdir) + $F3) 'inner.txt'
    Add-Case "$p-open-single-owner" $api 'open-single' '带所有者窗口的打开单个' (@($docs, '--owner=window') + $F3) 'alpha.txt'
    Add-Case "$p-open-multi" $api 'open-multi' '多选两个文件' (@($docs) + $F3) '"alpha.txt" "beta.txt"'
    Add-Case "$p-open-multi-one" $api 'open-multi' '多选模式只选一个文件（返回形态不同）' (@($docs) + $F3) '"gamma.txt"'
    Add-Case "$p-open-multi-cjk" $api 'open-multi' '多选含中文文件名' (@($docs) + $F3) '"报告.txt" "alpha.txt"'
    Add-Case "$p-open-filter-initial" $api 'open-single' '初始选中第 2 个过滤器（图片）' (@($docs, '--filter-index=2') + $F3) 'image.png'
    Add-Case "$p-open-filter-switch" $api 'open-single' '驱动切换到第 3 个过滤器（所有文件）' (@($docs) + $F3) 'notes.md' 3
    Add-Case "$p-open-cancel" $api 'open-single' '打开时点取消' (@($docs) + $F3) $null 0 'cancel'
    Add-Case "$p-save-default-name" $api 'save' '保存：默认文件名，直接确定' (@($docs, '--default-name=draft.txt') + $F3)
    Add-Case "$p-save-default-ext" $api 'save' '保存：默认扩展名 txt，输入无扩展名的 report' (@($docs, '--default-ext=txt') + $F3) 'report'
    Add-Case "$p-save-typed-ext" $api 'save' '保存：默认扩展名 txt，输入 report.md（保留输入的扩展名）' (@($docs, '--default-ext=txt') + $F3) 'report.md'
    Add-Case "$p-save-no-default-ext" $api 'save' '保存：不设默认扩展名，输入 data' (@($docs) + $F3) 'data'
    Add-Case "$p-save-filter-initial" $api 'save' '保存：初始选中第 2 个过滤器（图片），输入 data' (@($docs, '--filter-index=2') + $F3) 'data'
    Add-Case "$p-save-overwrite-yes" $api 'save' '保存已存在的 existing.txt：出现覆盖确认，选是' (@($docs, '--default-name=existing.txt') + $F3) $null 0 'ok' 'yes'
    Add-Case "$p-save-overwrite-no" $api 'save' '保存已存在的 existing.txt：出现覆盖确认，选否后取消' (@($docs, '--default-name=existing.txt') + $F3) $null 0 'ok' 'no'
    Add-Case "$p-save-overwrite-off" $api 'save' '保存已存在的 existing.txt：关闭覆盖确认，不应出现确认' (@($docs, '--default-name=existing.txt', '--overwrite-prompt=off') + $F3)
    Add-Case "$p-save-cancel" $api 'save' '保存时点取消' (@($docs, '--default-name=draft.txt') + $F3) $null 0 'cancel'
}
Add-Case 'ifd-folder' 'ifd' 'pick-folder' '选择文件夹（FOS_PICKFOLDERS）：输入子目录名' @($docs) 'sub'
Add-Case 'ifd-folder-initdir' 'ifd' 'pick-folder' '选择文件夹：初始目录是 sub，直接确定' @($subdir)
Add-Case 'ifd-folder-owner' 'ifd' 'pick-folder' '带所有者窗口的选择文件夹' @($docs, '--owner=window') 'sub'
Add-Case 'ifd-folder-cancel' 'ifd' 'pick-folder' '选择文件夹时点取消' @($docs) $null 0 'cancel'
Add-Case 'sbf-folder-new' 'sbf' 'pick-folder' 'SHBrowseForFolder 新样式：在编辑框输入完整路径' @($docs) '{DOCS}\sub'
Add-Case 'sbf-folder-new-initdir' 'sbf' 'pick-folder' 'SHBrowseForFolder 新样式：回调预选 sub，直接确定' @($subdir)
Add-Case 'sbf-folder-legacy-initdir' 'sbf' 'pick-folder' 'SHBrowseForFolder 旧样式（只有目录树）：回调预选 sub，直接确定' @($subdir, '--sbf-style=legacy')
Add-Case 'sbf-folder-owner' 'sbf' 'pick-folder' '带所有者窗口的 SHBrowseForFolder' @($docs, '--owner=window') '{DOCS}\sub'
Add-Case 'sbf-folder-cancel' 'sbf' 'pick-folder' 'SHBrowseForFolder 取消' @($docs) $null 0 'cancel'

# ---------------------------------------------------------------- 环境
function Get-OsInfo {
    $os = Get-CimInstance Win32_OperatingSystem
    $build = [int]$os.BuildNumber
    $family = if ($build -ge 22000) { 'windows11' } else { 'windows10' }
    $disp = ''
    try { $disp = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion').DisplayVersion } catch { }
    [pscustomobject]@{ family = $family; caption = $os.Caption.Trim(); build = $build; display_version = $disp }
}

function Get-HostExe($bits) {
    if ($bits -eq 'x86') { Join-Path $HostDir 'target\i686-pc-windows-msvc\release\dialog-host.exe' }
    else { Join-Path $HostDir 'target\release\dialog-host.exe' }
}

function Build-Host {
    foreach ($b in $Bits) {
        Write-Host "== 构建测试宿主 ($b)"
        Push-Location $HostDir
        try {
            if ($b -eq 'x86') { & cargo build --release --locked --jobs 6 --target i686-pc-windows-msvc }
            else { & cargo build --release --locked --jobs 6 }
            if ($LASTEXITCODE -ne 0) { throw "构建 $b 失败（退出码 $LASTEXITCODE）" }
        }
        finally { Pop-Location }
    }
}

# ---------------------------------------------------------------- 日志（一次启动一个文件，宿主与本脚本共用）
$script:LogFile = $null
function Start-Log {
    $dir = Join-Path $Root 'logs'
    New-Item -ItemType Directory -Force $dir | Out-Null
    $script:LogFile = Join-Path $dir ((Get-Date).ToString('yyyy-MM-dd HH-mm-ss') + '.log')
}
function Log($msg) {
    if (-not $script:LogFile) { return }
    $line = '{0} [dialog-baseline.ps1] {1}' -f (Get-Date).ToString('HH:mm:ss.fff'), $msg
    [IO.File]::AppendAllText($script:LogFile, $line + "`n", $Utf8)
}

# ---------------------------------------------------------------- 归一化与比对
function ConvertTo-Plain($o) {
    # 把 ConvertFrom-Json 的对象树转成 有序哈希表／List／标量，便于深拷贝与修改
    if ($null -eq $o) { return $null }
    if ($o -is [System.Management.Automation.PSCustomObject]) {
        $h = [ordered]@{}
        foreach ($p in $o.PSObject.Properties) { $h[$p.Name] = ConvertTo-Plain $p.Value }
        return $h
    }
    if ($o -is [System.Collections.IEnumerable] -and $o -isnot [string]) {
        $l = New-Object System.Collections.ArrayList
        foreach ($i in $o) { [void]$l.Add((ConvertTo-Plain $i)) }
        return , $l
    }
    return $o
}

function Normalize-Value($v, $fixtureRoot) {
    if ($v -is [string]) {
        $r = $v
        foreach ($variant in @($fixtureRoot, $fixtureRoot.Replace('\', '/'))) {
            $r = [regex]::Replace($r, [regex]::Escape($variant), '{FIXTURE}', 'IgnoreCase')
        }
        return $r
    }
    if ($v -is [System.Collections.IDictionary]) {
        $h = [ordered]@{}
        foreach ($k in $v.Keys) { $h[$k] = Normalize-Value $v[$k] $fixtureRoot }
        return $h
    }
    if ($v -is [System.Collections.IList]) {
        $l = New-Object System.Collections.ArrayList
        foreach ($i in $v) { [void]$l.Add((Normalize-Value $i $fixtureRoot)) }
        return , $l
    }
    return $v
}

function Flatten($v, $prefix, $into) {
    if ($v -is [System.Collections.IDictionary]) {
        if ($v.Count -eq 0) { $into[$prefix] = '{}'; return }
        foreach ($k in $v.Keys) { Flatten $v[$k] ($(if ($prefix) { "$prefix.$k" } else { "$k" })) $into }
    }
    elseif ($v -is [System.Collections.IList]) {
        if ($v.Count -eq 0) { $into[$prefix] = '[]'; return }
        for ($i = 0; $i -lt $v.Count; $i++) { Flatten $v[$i] "$prefix[$i]" $into }
    }
    elseif ($null -eq $v) { $into[$prefix] = 'null' }
    elseif ($v -is [bool]) { $into[$prefix] = $(if ($v) { 'true' } else { 'false' }) }
    elseif ($v -is [string]) { $into[$prefix] = '"' + $v + '"' }
    else { $into[$prefix] = [string]$v }
}

# 比较两份“结果文件”（record 的输出格式）。返回 差异对象列表；比较范围由 $caseIds 与 $bitsList 限定。
function Compare-Results($base, $act, $caseIds, $bitsList) {
    $diffs = New-Object System.Collections.ArrayList
    foreach ($id in $caseIds) {
        foreach ($b in $bitsList) {
            $bc = if ($base.cases.Contains($id)) { $base.cases[$id] } else { $null }
            $ac = if ($act.cases.Contains($id)) { $act.cases[$id] } else { $null }
            $be = if ($bc -and $bc.Contains($b)) { $bc[$b] } else { $null }
            $ae = if ($ac -and $ac.Contains($b)) { $ac[$b] } else { $null }
            if ($null -eq $be -and $null -eq $ae) { continue }
            if ($null -eq $be) { [void]$diffs.Add([pscustomobject]@{ Case = $id; Bits = $b; Field = '(整个用例)'; Base = '<基准没有该用例>'; Actual = '<有结果>' }); continue }
            if ($null -eq $ae) { [void]$diffs.Add([pscustomobject]@{ Case = $id; Bits = $b; Field = '(整个用例)'; Base = '<有基准>'; Actual = '<本次没有结果>' }); continue }
            $fb = [ordered]@{}; $fa = [ordered]@{}
            Flatten $be '' $fb; Flatten $ae '' $fa
            $keys = New-Object System.Collections.ArrayList
            foreach ($k in $fb.Keys) { [void]$keys.Add($k) }
            foreach ($k in $fa.Keys) { if (-not $fb.Contains($k)) { [void]$keys.Add($k) } }
            foreach ($k in $keys) {
                # 描述文字只是说明，不参与比对
                if ($k -eq 'description') { continue }
                $x = if ($fb.Contains($k)) { $fb[$k] } else { '<缺失>' }
                $y = if ($fa.Contains($k)) { $fa[$k] } else { '<缺失>' }
                if ($x -ne $y) { [void]$diffs.Add([pscustomobject]@{ Case = $id; Bits = $b; Field = $k; Base = $x; Actual = $y }) }
            }
        }
    }
    return , $diffs
}

function Show-Diffs($diffs, $compared) {
    if ($diffs.Count -eq 0) { Write-Host "全部一致：比对了 $compared 个用例×位数，没有字段差异。"; return }
    $last = ''
    foreach ($d in $diffs) {
        $head = "$($d.Case) [$($d.Bits)]"
        if ($head -ne $last) { Write-Host ''; Write-Host $head -ForegroundColor Yellow; $last = $head }
        Write-Host ("  {0}`n      基准: {1}`n      实际: {2}" -f $d.Field, $d.Base, $d.Actual)
    }
    $cases = @($diffs | ForEach-Object { "$($_.Case)|$($_.Bits)" } | Sort-Object -Unique).Count
    Write-Host ''
    Write-Host "共 $($diffs.Count) 处字段差异，涉及 $cases 个用例×位数（共比对 $compared 个）。" -ForegroundColor Red
}

function Read-ResultFile($path) {
    $raw = [IO.File]::ReadAllText($path, $Utf8)
    return ConvertTo-Plain (ConvertFrom-Json $raw)
}

# 紧凑、稳定的 JSON 输出（Windows PowerShell 5.1 的 ConvertTo-Json 缩进过深、文件很大，不适合入库）
function ConvertTo-JsonString([string]$s) {
    $sb = New-Object System.Text.StringBuilder
    [void]$sb.Append('"')
    foreach ($ch in $s.ToCharArray()) {
        switch ($ch) {
            '"' { [void]$sb.Append('\"') }
            '\' { [void]$sb.Append('\\') }
            "`n" { [void]$sb.Append('\n') }
            "`r" { [void]$sb.Append('\r') }
            "`t" { [void]$sb.Append('\t') }
            default {
                if ([int]$ch -lt 32) { [void]$sb.Append(('\u{0:x4}' -f [int]$ch)) } else { [void]$sb.Append($ch) }
            }
        }
    }
    [void]$sb.Append('"')
    return $sb.ToString()
}

function ConvertTo-PrettyJson($v, [int]$level = 0) {
    $pad = '  ' * ($level + 1)
    $end = '  ' * $level
    if ($null -eq $v) { return 'null' }
    if ($v -is [bool]) { return $(if ($v) { 'true' } else { 'false' }) }
    if ($v -is [string]) { return ConvertTo-JsonString $v }
    if ($v -is [System.Collections.IDictionary]) {
        if ($v.Count -eq 0) { return '{}' }
        $parts = foreach ($k in $v.Keys) { $pad + (ConvertTo-JsonString ([string]$k)) + ': ' + (ConvertTo-PrettyJson $v[$k] ($level + 1)) }
        return "{`n" + ($parts -join ",`n") + "`n$end}"
    }
    if ($v -is [System.Collections.IList]) {
        if ($v.Count -eq 0) { return '[]' }
        # 只含标量的短数组写在一行
        $scalar = $true
        foreach ($i in $v) { if ($i -is [System.Collections.IDictionary] -or $i -is [System.Collections.IList]) { $scalar = $false } }
        if ($scalar) { return '[' + ((@($v | ForEach-Object { ConvertTo-PrettyJson $_ ($level + 1) })) -join ', ') + ']' }
        $parts = foreach ($i in $v) { $pad + (ConvertTo-PrettyJson $i ($level + 1)) }
        return "[`n" + ($parts -join ",`n") + "`n$end]"
    }
    return [string]$v
}

function Write-ResultFile($obj, $path) {
    New-Item -ItemType Directory -Force (Split-Path $path) | Out-Null
    [IO.File]::WriteAllText($path, (ConvertTo-PrettyJson $obj) + "`n", $Utf8)
}

function Select-Cases {
    $c = @($Cases)
    if ($Filter) { $c = @($c | Where-Object { $_.Id -match $Filter }) }
    return $c
}

function Resolve-BaselinePath($os) {
    if ($Baseline) { return $Baseline }
    return Join-Path $BaselineDir ($os.family + '.json')
}

# ---------------------------------------------------------------- 驱动
function Invoke-Case($case, $bits, $fixture, $work) {
    $exe = Get-HostExe $bits
    $docsDir = Join-Path $fixture 'docs'
    $sub = { param($s) if ($null -eq $s) { $null } else { $s.Replace('{DOCS}', $docsDir).Replace('{FIXTURE}', $fixture) } }
    $outFile = Join-Path $work ("out\{0}-{1}.json" -f $case.Id, $bits)
    New-Item -ItemType Directory -Force (Split-Path $outFile) | Out-Null
    if (Test-Path $outFile) { Remove-Item $outFile -Force }

    $type = & $sub $case.Type
    if ($Perturb -and $case.Id -match '^ifd-open-single$') { $type = 'beta.txt' }
    $argv = @('run', $case.Api, $case.Scenario) + @($case.Args | ForEach-Object { & $sub $_ }) + @("--out=$outFile", "--log-file=$script:LogFile")
    # 每个参数整体加引号；路径不以反斜杠结尾，不会吃掉右引号
    $cmdline = ($argv | ForEach-Object { '"' + ($_ -replace '"', '\"') + '"' }) -join ' '

    $psi = New-Object Diagnostics.ProcessStartInfo $exe
    $psi.Arguments = $cmdline
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.StandardOutputEncoding = [Text.Encoding]::UTF8
    $p = [Diagnostics.Process]::Start($psi)
    $null = $p.Handle
    $procId = [uint32]$p.Id
    $events = New-Object System.Collections.ArrayList
    $err = $null
    $confirmShown = $false
    $confirmAnswer = $null
    $sw = [Diagnostics.Stopwatch]::StartNew()
    try {
        $dlg = [DialogDriver]::WaitDialog($procId, 15000, [IntPtr]::Zero)
        if ($dlg -eq [IntPtr]::Zero) { throw '对话框没有在 15 秒内出现' }
        [void]$events.Add("dialog-shown title=$([DialogDriver]::Text($dlg))")
        Start-Sleep -Milliseconds 600   # 等控件创建完成、焦点稳定
        if ($case.FilterSel -gt 0) {
            $ok = [DialogDriver]::SelectFilter($dlg, $case.FilterSel - 1)
            [void]$events.Add("select-filter $($case.FilterSel) ok=$ok")
            if (-not $ok) { throw "无法切换到第 $($case.FilterSel) 个过滤器" }
            Start-Sleep -Milliseconds 300
        }
        if ($null -ne $type) {
            $edit = [DialogDriver]::FileNameEdit($dlg)
            if ($edit -eq [IntPtr]::Zero) { throw '找不到文件名／路径输入框' }
            if (-not [DialogDriver]::SetText($edit, $type)) { throw '向输入框写入文字失败' }
            [void]$events.Add("typed [$type]")
            Start-Sleep -Milliseconds 200
        }
        $btn = if ($case.Action -eq 'cancel') { 2 } else { 1 }
        if (-not [DialogDriver]::Click($dlg, $btn)) { throw "找不到按钮 $btn" }
        [void]$events.Add("clicked $(if ($btn -eq 1) { 'ok' } else { 'cancel' })")
        if ($btn -eq 1) {
            # 等待：要么宿主结束，要么弹出确认框（覆盖确认等）
            $wait = [Diagnostics.Stopwatch]::StartNew()
            while ($wait.ElapsedMilliseconds -lt 4000 -and -not $p.HasExited) {
                $box = [DialogDriver]::WaitDialog($procId, 100, $dlg)
                if ($box -ne [IntPtr]::Zero) {
                    $confirmShown = $true
                    $yes = ($case.Confirm -ne 'no')
                    $how = [DialogDriver]::ClickConfirm($box, $yes)
                    $confirmAnswer = $(if ($yes) { 'yes' } else { 'no' })
                    [void]$events.Add("confirm-shown title=$([DialogDriver]::Text($box)) answer=$confirmAnswer via=$how")
                    if (-not $yes) {
                        # 选“否”后回到主对话框，再取消，结束这次调用
                        Start-Sleep -Milliseconds 500
                        [void][DialogDriver]::Click($dlg, 2)
                        [void]$events.Add('clicked cancel after no')
                    }
                    break
                }
            }
        }
        if (-not $p.WaitForExit($CaseTimeoutSec * 1000)) { throw "宿主 $CaseTimeoutSec 秒内没有结束" }
    }
    catch { $err = $_.Exception.Message }
    finally {
        if (-not $p.HasExited) { try { $p.Kill() } catch { } }
        try { $null = $p.StandardOutput.ReadToEnd() } catch { }
    }
    Log ("case {0} [{1}] {2}ms exit={3} err={4} events={5}" -f $case.Id, $bits, $sw.ElapsedMilliseconds, $(try { $p.ExitCode } catch { '?' }), $err, ($events -join '; '))
    if (-not $err -and -not (Test-Path $outFile)) { $err = "宿主没有写出 $outFile（退出码 $($p.ExitCode)）" }
    if ($err) {
        return [ordered]@{ error = $err; driver = $events }
    }
    $rec = Read-ResultFile $outFile
    $entry = [ordered]@{
        description = $case.Desc
        request     = Normalize-Value $rec['request'] $fixture
        observed    = [ordered]@{ overwrite_confirm_shown = $confirmShown; overwrite_confirm_answer = $confirmAnswer }
        result      = Normalize-Value $rec['result'] $fixture
    }
    return $entry
}

function Invoke-AllCases($selected) {
    $selected = @($selected)
    if (-not $WorkDir) { $WorkDir = Join-Path $env:TEMP 'sfm-dialog-host\baseline' }
    $fixture = Join-Path $WorkDir 'fixture'
    if (-not ('DialogDriver' -as [type])) {
        Add-Type -TypeDefinition ([IO.File]::ReadAllText((Join-Path $PSScriptRoot 'dialog-driver.cs'), $Utf8))
    }
    $os = Get-OsInfo
    $results = [ordered]@{}
    $failed = 0
    foreach ($b in $Bits) {
        $exe = Get-HostExe $b
        if (-not (Test-Path $exe)) { throw "找不到 $exe；先构建（去掉 -SkipBuild）" }
    }
    $n = 0
    foreach ($case in $selected) {
        foreach ($b in $Bits) {
            $n++
            # 每个用例前重建夹具，保证互不影响（宿主拒绝在 %TEMP%\sfm-dialog-host 之外重建）
            $exe = Get-HostExe $b
            $fx = & $exe fixture --root=$fixture
            if ($LASTEXITCODE -ne 0) { throw "重建夹具失败：$fx" }
            Write-Host ("[{0}/{1}] {2} ({3})" -f $n, ($selected.Count * $Bits.Count), $case.Id, $b)
            $entry = Invoke-Case $case $b $fixture $WorkDir
            if ($entry.Contains('error')) { $failed++; Write-Host "  失败：$($entry.error)" -ForegroundColor Red }
            if (-not $results.Contains($case.Id)) { $results[$case.Id] = [ordered]@{} }
            $results[$case.Id][$b] = $entry
        }
    }
    return [pscustomobject]@{
        Doc    = [ordered]@{
            schema = 1; kind = 'dialog-baseline'
            os = [ordered]@{ family = $os.family; caption = $os.caption; build = $os.build; display_version = $os.display_version }
            host = [ordered]@{ tool = 'dialog-host'; schema = 1 }
            placeholders = [ordered]@{ '{FIXTURE}' = '测试夹具根目录（%TEMP%\sfm-dialog-host\...\fixture）' }
            cases = $results
        }
        Failed = $failed
    }
}

# ---------------------------------------------------------------- 模式
switch ($Mode) {
    'list' {
        Write-Host ('{0,-28} {1,-5} {2,-12} {3}' -f '用例', '方式', '场景', '说明')
        foreach ($c in (Select-Cases)) {
            $act = if ($c.Action -eq 'cancel') { '（取消）' } else { '' }
            Write-Host ('{0,-28} {1,-5} {2,-12} {3}{4}' -f $c.Id, $c.Api, $c.Scenario, $c.Desc, $act)
        }
        Write-Host ("共 {0} 个用例，每个按 {1} 各跑一次。" -f @(Select-Cases).Count, ($Bits -join '、'))
        exit 0
    }
    'record' {
        if (-not $SkipBuild) { Build-Host }
        Start-Log
        $os = Get-OsInfo
        $selected = Select-Cases
        Write-Host "记录基准：$($os.caption) build $($os.build)，$(@($selected).Count) 个用例 × $($Bits -join '、')"
        $r = Invoke-AllCases $selected
        if ($r.Failed -gt 0) { Write-Host "有 $($r.Failed) 个用例执行失败，不写入基准。" -ForegroundColor Red; exit 1 }
        $path = Resolve-BaselinePath $os
        if ($Filter -or $Bits.Count -lt 2) {
            # 局部记录：合并进已有基准，不覆盖其他用例
            if (Test-Path $path) {
                $old = Read-ResultFile $path
                foreach ($id in $r.Doc.cases.Keys) {
                    if (-not $old.cases.Contains($id)) { $old.cases[$id] = [ordered]@{} }
                    foreach ($b in $r.Doc.cases[$id].Keys) { $old.cases[$id][$b] = $r.Doc.cases[$id][$b] }
                }
                $r.Doc.cases = $old.cases
            }
        }
        Write-ResultFile $r.Doc $path
        Write-Host "基准已写入 $path"
        Log "baseline written $path"
        exit 0
    }
    'compare' {
        $os = Get-OsInfo
        $path = Resolve-BaselinePath $os
        if (-not (Test-Path $path)) {
            Write-Host "未覆盖：$($os.caption)（build $($os.build)）没有基准文件 $path。" -ForegroundColor Yellow
            Write-Host '不会用其他系统的基准代替；请在该系统上运行 -Mode record 记录基准。'
            exit 2
        }
        if (-not $SkipBuild) { Build-Host }
        Start-Log
        $selected = Select-Cases
        $r = Invoke-AllCases $selected
        if ($Out) { Write-ResultFile $r.Doc $Out; Write-Host "本次结果已写入 $Out" }
        $base = Read-ResultFile $path
        if ($base.os.family -ne $os.family) { Write-Host "基准系统 $($base.os.family) 与当前 $($os.family) 不同，无法判断。" -ForegroundColor Yellow; exit 2 }
        $ids = @($selected | ForEach-Object { $_.Id })
        $diffs = Compare-Results $base $r.Doc $ids $Bits
        # 执行失败的用例也算差异（Compare 看到的是 error 字段）
        Show-Diffs $diffs ($ids.Count * $Bits.Count)
        Log "compare done diffs=$($diffs.Count)"
        if ($r.Failed -gt 0) { Write-Host "有 $($r.Failed) 个用例执行失败（见上）。" -ForegroundColor Red }
        exit $(if ($diffs.Count -eq 0 -and $r.Failed -eq 0) { 0 } else { 1 })
    }
    'diff' {
        if (-not $Actual) { throw 'diff 模式需要 -Actual <结果文件>' }
        $act = Read-ResultFile $Actual
        $os = Get-OsInfo
        $family = $act.os.family
        $path = if ($Baseline) { $Baseline } else { Join-Path $BaselineDir ($family + '.json') }
        if (-not (Test-Path $path)) {
            Write-Host "未覆盖：没有 $family 的基准文件 $path。" -ForegroundColor Yellow
            exit 2
        }
        $base = Read-ResultFile $path
        $ids = New-Object System.Collections.ArrayList
        foreach ($k in $base.cases.Keys) { [void]$ids.Add($k) }
        foreach ($k in $act.cases.Keys) { if ($ids -notcontains $k) { [void]$ids.Add($k) } }
        if ($Filter) { $ids = @($ids | Where-Object { $_ -match $Filter }) }
        $diffs = Compare-Results $base $act $ids $Bits
        Show-Diffs $diffs (@($ids).Count * $Bits.Count)
        exit $(if ($diffs.Count -eq 0) { 0 } else { 1 })
    }
    'selftest' {
        $os = Get-OsInfo
        $path = if ($Baseline) { $Baseline } else { Join-Path $BaselineDir 'windows10.json' }
        if (-not (Test-Path $path)) { throw "selftest 需要基准文件 $path" }
        $base = Read-ResultFile $path
        $ids = @($base.cases.Keys)
        function Clone($o) { ConvertTo-Plain (ConvertFrom-Json (ConvertTo-PrettyJson $o)) }
        $mutations = @(
            @{ Name = '改一个返回路径'; Case = 'ifd-open-single'; Bits = 'x64'; Expect = 'result.paths[0]'; Do = { param($e) $e.result.paths[0] = $e.result.paths[0] + 'X' } },
            @{ Name = '多选少一个文件'; Case = 'gofn-open-multi'; Bits = 'x86'; Expect = 'result.paths[1]'; Do = { param($e) $e.result.paths.RemoveAt(1) } },
            @{ Name = '取消变成未取消'; Case = 'sbf-folder-cancel'; Bits = 'x64'; Expect = 'result.cancelled'; Do = { param($e) $e.result.cancelled = $false } },
            @{ Name = '过滤器序号不同'; Case = 'ifd-open-filter-switch'; Bits = 'x64'; Expect = 'result.filter_index'; Do = { param($e) $e.result.filter_index = 1 } },
            @{ Name = '默认扩展名没补上'; Case = 'ifd-save-default-ext'; Bits = 'x64'; Expect = 'result.files[0].extension'; Do = { param($e) $e.result.files[0].extension = '' } },
            @{ Name = '最终文件名不同'; Case = 'gofn-save-default-ext'; Bits = 'x86'; Expect = 'result.files[0].name'; Do = { param($e) $e.result.files[0].name = 'report' } },
            @{ Name = '覆盖确认没出现'; Case = 'ifd-save-overwrite-yes'; Bits = 'x64'; Expect = 'observed.overwrite_confirm_shown'; Do = { param($e) $e.observed.overwrite_confirm_shown = $false } },
            @{ Name = '返回码不同'; Case = 'gofn-open-single'; Bits = 'x64'; Expect = 'result.return_code'; Do = { param($e) $e.result.return_code = 'FALSE' } },
            @{ Name = '缓冲区布局不同（旧式接口特有字段）'; Case = 'gofn-open-multi-one'; Bits = 'x64'; Expect = 'result.extras.buffer_layout'; Do = { param($e) $e.result.extras.buffer_layout = 'dir+names' } },
            @{ Name = '请求参数不同'; Case = 'ifd-open-initdir-sub'; Bits = 'x64'; Expect = 'request.init_dir'; Do = { param($e) $e.request.init_dir = '{FIXTURE}\docs' } },
            @{ Name = '去掉一个用例的某位数结果'; Case = 'sbf-folder-new'; Bits = 'x86'; Expect = '(整个用例)'; Do = $null }
        )
        $fail = 0
        $same = Compare-Results $base (Clone $base) $ids @('x64', 'x86')
        if ($same.Count -eq 0) { Write-Host '[通过] 未改动时：0 处差异' } else { Write-Host "[失败] 未改动却报告了 $($same.Count) 处差异" -ForegroundColor Red; $fail++ }
        foreach ($m in $mutations) {
            if (-not $base.cases.Contains($m.Case) -or -not $base.cases[$m.Case].Contains($m.Bits)) {
                Write-Host "[失败] $($m.Name)：基准里没有 $($m.Case) [$($m.Bits)]" -ForegroundColor Red; $fail++; continue
            }
            $mut = Clone $base
            if ($m.Do) { & $m.Do $mut.cases[$m.Case][$m.Bits] } else { $mut.cases[$m.Case].Remove($m.Bits) }
            $d = Compare-Results $base $mut $ids @('x64', 'x86')
            $hit = @($d | Where-Object { $_.Case -eq $m.Case -and $_.Bits -eq $m.Bits -and $_.Field.Contains($m.Expect) })
            $others = @($d | Where-Object { $_.Case -ne $m.Case -or $_.Bits -ne $m.Bits })
            if ($hit.Count -ge 1 -and $others.Count -eq 0) {
                Write-Host ("[检出] {0}：{1} [{2}] 字段 {3}：基准 {4} → 实际 {5}" -f $m.Name, $m.Case, $m.Bits, $hit[0].Field, $hit[0].Base, $hit[0].Actual)
            }
            else {
                Write-Host ("[漏检] {0}：期望在 {1} [{2}] 检出 {3}，实得 {4} 处差异" -f $m.Name, $m.Case, $m.Bits, $m.Expect, $d.Count) -ForegroundColor Red
                $fail++
            }
        }
        if ($fail -eq 0) { Write-Host "selftest 通过：$($mutations.Count) 种人为差异全部逐字段检出。" } else { Write-Host "selftest 失败：$fail 项。" -ForegroundColor Red }
        exit $(if ($fail -eq 0) { 0 } else { 1 })
    }
}
