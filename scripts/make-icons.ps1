# 生成 Blurt 应用图标（无需外部工具，纯 GDI+）
# 输出到 src-tauri/icons/
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$outDir = Join-Path $PSScriptRoot '..\src-tauri\icons'
New-Item -ItemType Directory -Force $outDir | Out-Null
$outDir = (Resolve-Path $outDir).Path

function Add-RoundedRect([System.Drawing.Drawing2D.GraphicsPath]$path, [float]$x, [float]$y, [float]$w, [float]$h, [float]$r) {
    $d = $r * 2
    $path.StartFigure()
    $path.AddArc($x, $y, $d, $d, 180, 90)
    $path.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $path.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $path.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $path.CloseFigure()
}

function New-BlurtBitmap([int]$size) {
    $bmp = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.Clear([System.Drawing.Color]::Transparent)
    $s = $size / 1024.0

    # 圆角方形底，靛蓝→紫 渐变
    $m = 32 * $s
    $side = (1024 - 64) * $s
    $bgPath = New-Object System.Drawing.Drawing2D.GraphicsPath
    Add-RoundedRect $bgPath $m $m $side $side (232 * $s)
    $rect = New-Object System.Drawing.RectangleF($m, $m, $side, $side)
    $c1 = [System.Drawing.Color]::FromArgb(255, 0x63, 0x66, 0xF1)
    $c2 = [System.Drawing.Color]::FromArgb(255, 0xA8, 0x55, 0xF7)
    $brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush($rect, $c1, $c2, [System.Drawing.Drawing2D.LinearGradientMode]::ForwardDiagonal)
    $g.FillPath($brush, $bgPath)

    # 白色声波条（小尺寸 3 条，大尺寸 5 条）
    $white = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(245, 255, 255, 255))
    if ($size -le 48) { $heights = @(400, 620, 400) } else { $heights = @(300, 480, 660, 480, 300) }
    $n = $heights.Count
    $bw = 96.0; $gap = 64.0
    $total = $n * $bw + ($n - 1) * $gap
    $x0 = (1024 - $total) / 2.0
    for ($i = 0; $i -lt $n; $i++) {
        $h = $heights[$i]
        $x = ($x0 + $i * ($bw + $gap)) * $s
        $y = (512 - $h / 2.0) * $s
        $p = New-Object System.Drawing.Drawing2D.GraphicsPath
        Add-RoundedRect $p $x $y ($bw * $s) ($h * $s) ($bw * $s / 2.0)
        $g.FillPath($white, $p)
        $p.Dispose()
    }
    $white.Dispose(); $brush.Dispose(); $bgPath.Dispose(); $g.Dispose()
    return $bmp
}

function Save-Png([int]$size, [string]$name) {
    $bmp = New-BlurtBitmap $size
    $bmp.Save((Join-Path $outDir $name), [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Host "  $name"
}

Write-Host "生成 PNG..."
Save-Png 1024 'icon.png'
Save-Png 32   '32x32.png'
Save-Png 128  '128x128.png'
Save-Png 256  '128x128@2x.png'
Save-Png 64   'tray.png'

# 组装 icon.ico（内嵌 PNG 条目，Vista+ 格式）
Write-Host "生成 icon.ico..."
$sizes = @(16, 20, 24, 32, 48, 64, 128, 256)
$pngs = @()
foreach ($sz in $sizes) {
    $bmp = New-BlurtBitmap $sz
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $pngs += , $ms.ToArray()
    $ms.Dispose(); $bmp.Dispose()
}
$ico = New-Object System.IO.MemoryStream
$w = New-Object System.IO.BinaryWriter($ico)
$w.Write([uint16]0); $w.Write([uint16]1); $w.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $sz = $sizes[$i]
    $b = if ($sz -ge 256) { [byte]0 } else { [byte]$sz }
    $w.Write($b); $w.Write($b)            # width, height (0 = 256)
    $w.Write([byte]0); $w.Write([byte]0)  # colors, reserved
    $w.Write([uint16]1); $w.Write([uint16]32)  # planes, bpp
    $w.Write([uint32]$pngs[$i].Length)
    $w.Write([uint32]$offset)
    $offset += $pngs[$i].Length
}
foreach ($p in $pngs) { $w.Write($p) }
[System.IO.File]::WriteAllBytes((Join-Path $outDir 'icon.ico'), $ico.ToArray())
$w.Dispose(); $ico.Dispose()

Write-Host "✓ 图标生成完毕：$outDir" -ForegroundColor Green
