$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
$assetPath = Join-Path (Split-Path -Parent $PSScriptRoot) 'assets'
$source = [System.Drawing.Bitmap]::new((Join-Path $assetPath 'logo-source.png'))
try {
    # Trim transparent padding without changing the supplied artwork.
    $left = $source.Width; $top = $source.Height; $right = 0; $bottom = 0
    for ($y = 0; $y -lt $source.Height; $y += 2) {
        for ($x = 0; $x -lt $source.Width; $x += 2) {
            if ($source.GetPixel($x,$y).A -gt 24) {
                $left = [Math]::Min($left,$x); $right = [Math]::Max($right,$x)
                $top = [Math]::Min($top,$y); $bottom = [Math]::Max($bottom,$y)
            }
        }
    }
    $crop = [System.Drawing.Rectangle]::new($left,$top,($right-$left+1),($bottom-$top+1))
    $frames = @()
    foreach ($side in @(16,24,32,48,64,128,256)) {
        $bitmap = [System.Drawing.Bitmap]::new($side,$side)
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
        $graphics.Clear([System.Drawing.Color]::Transparent)
        $scale = ($side * 0.94) / [Math]::Max($crop.Width,$crop.Height)
        $width = [int]($crop.Width*$scale); $height = [int]($crop.Height*$scale)
        $rect = [System.Drawing.Rectangle]::new([int](($side-$width)/2),[int](($side-$height)/2),$width,$height)
        $graphics.DrawImage($source,$rect,$crop,[System.Drawing.GraphicsUnit]::Pixel)
        $stream = [System.IO.MemoryStream]::new()
        $bitmap.Save($stream,[System.Drawing.Imaging.ImageFormat]::Png)
        $frames += ,@{Side=$side;Bytes=$stream.ToArray()}
        if ($side -eq 256) { $bitmap.Save((Join-Path $assetPath 'icon.png'),[System.Drawing.Imaging.ImageFormat]::Png) }
        $graphics.Dispose(); $bitmap.Dispose(); $stream.Dispose()
    }
    $output = [System.IO.File]::Create((Join-Path $assetPath 'app.ico'))
    $writer = [System.IO.BinaryWriter]::new($output)
    $writer.Write([uint16]0); $writer.Write([uint16]1); $writer.Write([uint16]$frames.Count)
    $offset = 6 + $frames.Count * 16
    foreach ($frame in $frames) {
        $dimension = if ($frame.Side -eq 256) {0} else {$frame.Side}
        $writer.Write([byte]$dimension); $writer.Write([byte]$dimension)
        $writer.Write([byte]0); $writer.Write([byte]0); $writer.Write([uint16]1); $writer.Write([uint16]32)
        $writer.Write([uint32]$frame.Bytes.Length); $writer.Write([uint32]$offset)
        $offset += $frame.Bytes.Length
    }
    foreach ($frame in $frames) { $writer.Write([byte[]]$frame.Bytes) }
    $writer.Dispose(); $output.Dispose()
} finally { $source.Dispose() }
