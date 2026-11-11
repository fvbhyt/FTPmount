# FTP 挂载控制台 —— PowerShell + WinForms，零依赖
# 用法：双击「启动GUI.bat」，或 powershell -ExecutionPolicy Bypass -File gui.ps1
# 说明：挂载盘符需要管理员权限；建议右键「以管理员身份运行」启动GUI.bat

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
[System.Windows.Forms.Application]::EnableVisualStyles()

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$exe  = Join-Path $root 'target\release\ftpmount-rs.exe'
if (-not (Test-Path $exe)) {
    [System.Windows.Forms.MessageBox]::Show("找不到:`n$exe`n`n请先在 $root 下执行: cargo build --release", '错误')
    exit 1
}

$form = New-Object System.Windows.Forms.Form
$form.Text = 'FTP 挂载控制台'
$form.ClientSize = New-Object System.Drawing.Size(820, 610)
$form.StartPosition = 'CenterScreen'
$form.FormBorderStyle = 'FixedSingle'
$form.MaximizeBox = $false
$form.Font = New-Object System.Drawing.Font('Microsoft YaHei UI', 9)

$script:jobs = @()
$script:mountProc = $null

# ---------------- 控件辅助 ----------------
function Add-Label($parent, $text, $x, $y, $w = 70) {
    $l = New-Object System.Windows.Forms.Label
    $l.Text = $text
    $l.Location = New-Object System.Drawing.Point($x, $y)
    $l.Size = New-Object System.Drawing.Size($w, 22)
    $parent.Controls.Add($l)
    return $l
}
function Add-Text($parent, $x, $y, $w, $text, $pw = $false) {
    $t = New-Object System.Windows.Forms.TextBox
    $t.Location = New-Object System.Drawing.Point($x, $y)
    $t.Size = New-Object System.Drawing.Size($w, 24)
    $t.Text = $text
    if ($pw) { $t.UseSystemPasswordChar = $true }
    $parent.Controls.Add($t)
    return $t
}

# ---------------- 连接设置 ----------------
$grp = New-Object System.Windows.Forms.GroupBox
$grp.Text = '连接设置'
$grp.Location = New-Object System.Drawing.Point(12, 10)
$grp.Size = New-Object System.Drawing.Size(796, 178)
$form.Controls.Add($grp)

Add-Label $grp '服务器' 12 28 | Out-Null
$txtHost = Add-Text $grp 64 25 150 '192.168.1.10'
Add-Label $grp '端口' 226 28 36 | Out-Null
$txtPort = Add-Text $grp 264 25 55 '21'
Add-Label $grp '用户' 332 28 36 | Out-Null
$txtUser = Add-Text $grp 370 25 110 'yourname'
Add-Label $grp '密码' 494 28 36 | Out-Null
$txtPass = Add-Text $grp 532 25 130 '' $true

Add-Label $grp '远端目录' 12 62 62 | Out-Null
$txtRemote = Add-Text $grp 78 59 400 '/public'
Add-Label $grp '盘符' 496 62 36 | Out-Null
$cmbDrive = New-Object System.Windows.Forms.ComboBox
$cmbDrive.DropDownStyle = 'DropDown'
$cmbDrive.Location = New-Object System.Drawing.Point(534, 59)
$cmbDrive.Size = New-Object System.Drawing.Size(80, 24)
[void]$cmbDrive.Items.AddRange(@('R:','S:','T:','U:','V:','W:','X:','Y:','Z:'))
$cmbDrive.Text = 'R:'
$grp.Controls.Add($cmbDrive)

$chkDebug = New-Object System.Windows.Forms.CheckBox
$chkDebug.Text = '详细日志（勾选后记录每条 FTP 动作，排障用）'
$chkDebug.Location = New-Object System.Drawing.Point(12, 138)
$chkDebug.Size = New-Object System.Drawing.Size(360, 26)
$chkDebug.Font = New-Object System.Drawing.Font('Microsoft YaHei UI', 9)
$grp.Controls.Add($chkDebug)

$lblHint = New-Object System.Windows.Forms.Label
$lblHint.Text = '提示：挂载盘符需管理员权限。远端目录必须是该账号可读写的路径。'
$lblHint.Location = New-Object System.Drawing.Point(12, 108)
$lblHint.Size = New-Object System.Drawing.Size(770, 22)
$lblHint.ForeColor = [System.Drawing.Color]::DimGray
$grp.Controls.Add($lblHint)

# ---------------- 按钮 ----------------
function Add-Btn($text, $x, $y, $w, $h) {
    $b = New-Object System.Windows.Forms.Button
    $b.Text = $text
    $b.Location = New-Object System.Drawing.Point($x, $y)
    $b.Size = New-Object System.Drawing.Size($w, $h)
    $form.Controls.Add($b)
    return $b
}
$btnCheck   = Add-Btn '检测连通' 12 196 100 34
$btnMount   = Add-Btn '挂载'     122 196 100 34
$btnUnmount = Add-Btn '卸载'     232 196 100 34
$btnOpen    = Add-Btn '打开盘符' 342 196 100 34
$btnRw      = Add-Btn '读写测试' 452 196 100 34
$btnSelf    = Add-Btn '逻辑自测' 562 196 100 34
$btnClear   = Add-Btn '清空日志' 672 196 100 34

# ---------------- 状态 & 日志 ----------------
$lblStatus = New-Object System.Windows.Forms.Label
$lblStatus.Location = New-Object System.Drawing.Point(12, 236)
$lblStatus.Size = New-Object System.Drawing.Size(796, 24)
$lblStatus.Font = New-Object System.Drawing.Font('Microsoft YaHei UI', 10, [System.Drawing.FontStyle]::Bold)
$lblStatus.Text = '● 未挂载'
$lblStatus.ForeColor = [System.Drawing.Color]::Gray
$form.Controls.Add($lblStatus)

$log = New-Object System.Windows.Forms.RichTextBox
$log.Location = New-Object System.Drawing.Point(12, 264)
$log.Size = New-Object System.Drawing.Size(796, 334)
$log.ReadOnly = $true
$log.BackColor = [System.Drawing.Color]::FromArgb(250, 250, 250)
$log.Font = New-Object System.Drawing.Font('Consolas', 9)
$form.Controls.Add($log)

# ---------------- 核心函数 ----------------
function Append-Log([string]$text) {
    if ($log.InvokeRequired) {
        $log.Invoke([Action]{ Append-Log $text }) | Out-Null
        return
    }
    if ([string]::IsNullOrEmpty($text)) { return }
    $log.AppendText($text + "`r`n")
    $log.SelectionStart = $log.TextLength
    $log.ScrollToCaret()
}
function Set-Status([string]$text, [string]$color) {
    if ($lblStatus.InvokeRequired) {
        $lblStatus.Invoke([Action]{ Set-Status $text $color }) | Out-Null
        return
    }
    $lblStatus.Text = $text
    $lblStatus.ForeColor = [System.Drawing.Color]::FromName($color)
}
function Write-Toml {
    $port = 21
    [void][int]::TryParse($txtPort.Text.Trim(), [ref]$port)
    $t = @"
[ftp]
host = "$($txtHost.Text.Trim())"
port = $port
user = "$($txtUser.Text.Trim())"
pass = "$($txtPass.Text)"
remote_path = "$($txtRemote.Text.Trim())"
drive = "$($cmbDrive.Text.Trim())"
volname = "FTP Mount"
list_tz_offset = 480
tls = false
no_check_certificate = false
"@
    $path = Join-Path $env:TEMP ("ftpmount_gui_{0}.toml" -f $PID)
    [System.IO.File]::WriteAllText($path, $t, (New-Object System.Text.UTF8Encoding($false)))
    return $path
}
function Read-Text([string]$path) {
    try {
        $fs = [System.IO.File]::Open($path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
        try {
            $sr = New-Object System.IO.StreamReader($fs, [System.Text.Encoding]::UTF8)
            $t = $sr.ReadToEnd()
            $sr.Dispose()
            return $t
        } finally { $fs.Dispose() }
    } catch { return '' }
}
function Start-Tool([string[]]$ArgList, [string]$tag) {
    $stamp = [DateTime]::Now.ToString('HHmmssfff')
    $out = Join-Path $env:TEMP ("ftpmount_{0}_{1}.out" -f $tag, $stamp)
    $err = Join-Path $env:TEMP ("ftpmount_{0}_{1}.err" -f $tag, $stamp)
    if ($chkDebug.Checked) { $env:FT_DEBUG = '1' } else { Remove-Item Env:\FT_DEBUG -ErrorAction SilentlyContinue }
    $p = Start-Process -FilePath $exe -ArgumentList $ArgList -RedirectStandardOutput $out -RedirectStandardError $err -NoNewWindow -PassThru -WorkingDirectory $root
    $job = [pscustomobject]@{ Tag = $tag; Proc = $p; Out = $out; Err = $err; OutLen = 0; ErrLen = 0; Done = $false }
    $script:jobs += $job
    if ($tag -eq 'mount') { $script:mountProc = $p }
    return $p
}

# ---------------- 按钮行为 ----------------
$btnCheck.add_Click({
    $toml = Write-Toml
    Append-Log '=== 检测连通性 ==='
    Set-Status '● 检测中…' 'DarkOrange'
    [void](Start-Tool @('check', $toml) 'check')
})

$btnMount.add_Click({
    if ($script:mountProc -and -not $script:mountProc.HasExited) {
        [void][System.Windows.Forms.MessageBox]::Show('已经处于挂载状态，请先点「卸载」。', '提示')
        return
    }
    $drive = $cmbDrive.Text.Trim().ToUpper()
    if ($drive -notmatch '^[A-Z]:$') {
        [void][System.Windows.Forms.MessageBox]::Show('盘符格式应为 R: 这样的形式。', '提示')
        return
    }
    if (Test-Path ($drive + '\')) {
        [void][System.Windows.Forms.MessageBox]::Show("盘符 $drive 已被占用，请换一个。", '提示')
        return
    }
    $toml = Write-Toml
    Append-Log "=== 挂载 $drive  ->  ftp://$($txtHost.Text.Trim()):$($txtPort.Text.Trim())$($txtRemote.Text.Trim()) ==="
    Set-Status "● 挂载中…（$drive）" 'DarkOrange'
    [void](Start-Tool @('mount', $toml, $drive) 'mount')
})

$btnUnmount.add_Click({
    if ($script:mountProc -and -not $script:mountProc.HasExited) {
        try { $script:mountProc.Kill() } catch { }
        Append-Log '已请求卸载。'
        Set-Status '● 已卸载' 'Gray'
    } else {
        [void][System.Windows.Forms.MessageBox]::Show('当前没有挂载进程。', '提示')
    }
})

$btnOpen.add_Click({
    $drive = $cmbDrive.Text.Trim().ToUpper()
    if (Test-Path ($drive + '\')) {
        Start-Process explorer.exe -ArgumentList ($drive + '\')
    } else {
        [void][System.Windows.Forms.MessageBox]::Show("$drive 还没有挂载。", '提示')
    }
})

$btnRw.add_Click({
    $drive = $cmbDrive.Text.Trim().ToUpper()
    if (-not (Test-Path ($drive + '\'))) {
        [void][System.Windows.Forms.MessageBox]::Show("$drive 还没有挂载。", '提示')
        return
    }
    $base = $drive + '\_gui_test.txt'
    Append-Log '=== 读写测试（覆盖短/长内容，专测乱码） ==='
    try {
        [System.IO.File]::WriteAllText($base, 'hello-123')
        $r1 = [System.IO.File]::ReadAllText($base)
        [System.IO.File]::WriteAllText($base, 'x')
        $r2 = [System.IO.File]::ReadAllText($base)
        [System.IO.File]::WriteAllText($base, 'longer-content-456789')
        $r3 = [System.IO.File]::ReadAllText($base)
        [System.IO.File]::Delete($base)
        $gone = -not (Test-Path $base)
        Append-Log ("  首次写入 = {0}" -f $r1)
        Append-Log ("  覆盖为短内容 = {0}" -f $r2)
        Append-Log ("  覆盖为长内容 = {0}" -f $r3)
        Append-Log ("  删除成功 = {0}" -f $gone)
        if ($r1 -eq 'hello-123' -and $r2 -eq 'x' -and $r3 -eq 'longer-content-456789' -and $gone) {
            Set-Status '● 读写测试通过（无乱码、可覆盖、可删除）' 'Green'
        } else {
            Set-Status '● 读写测试失败，详见日志' 'Red'
        }
    } catch {
        Append-Log ('  异常: ' + $_.Exception.Message)
        Set-Status '● 读写测试异常' 'Red'
    }
})

$btnSelf.add_Click({
    $toml = Write-Toml
    Append-Log '=== 逻辑层自测（覆盖/删除/区间读，会临时写测试文件） ==='
    Set-Status '● 自测运行中…' 'DarkOrange'
    [void](Start-Tool @('fstest', $toml) 'fstest')
})

$btnClear.add_Click({ $log.Clear() })

# ---------------- 日志轮询（避免事件泵问题） ----------------
$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 400
$timer.add_Tick({
    foreach ($j in $script:jobs) {
        $checks = @(
            [pscustomobject]@{ Path = $j.Out; Prop = 'OutLen' },
            [pscustomobject]@{ Path = $j.Err; Prop = 'ErrLen' }
        )
        foreach ($c in $checks) {
            $txt = Read-Text $c.Path
            $old = $j.($c.Prop)
            if ($txt.Length -gt $old) {
                $new = $txt.Substring($old)
                $j.PSObject.Properties[$c.Prop].Value = $txt.Length
                $new = $new.TrimEnd("`r", "`n")
                if ($new) {
                    Append-Log $new
                    if ($j.Tag -eq 'mount' -and $new -match '已挂载') {
                        Set-Status ("● 已挂载 {0}" -f $cmbDrive.Text.Trim().ToUpper()) 'Green'
                    }
                }
            }
        }
        if ($j.Proc.HasExited -and -not $j.Done) {
            $j.Done = $true
            $code = $j.Proc.ExitCode
            switch ($j.Tag) {
                'mount'  { Set-Status '● 挂载进程已退出（卸载或失败）' 'Gray' }
                'check'  { if ($code -eq 0) { Set-Status '● 检测通过' 'Green' } else { Set-Status ("● 检测失败（退出码 $code）") 'Red' } }
                'fstest' { if ($code -eq 0) { Set-Status '● 自测全部通过' 'Green' } else { Set-Status ("● 自测有失败项（退出码 $code）") 'Red' } }
            }
        }
    }
})
$timer.Start()

$form.add_FormClosing({
    $timer.Stop()
    if ($script:mountProc -and -not $script:mountProc.HasExited) {
        try { $script:mountProc.Kill() } catch { }
    }
})

Append-Log '就绪。填写参数后点「检测连通」，通过后点「挂载」。'
[void]$form.ShowDialog()
