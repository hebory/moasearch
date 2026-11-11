param([string]$RepositoryRoot = (Split-Path $PSScriptRoot -Parent))
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName WindowsBase, PresentationCore, PresentationFramework
$root = [IO.Path]::GetFullPath($RepositoryRoot)
[xml]$source = Get-Content -LiteralPath (Join-Path $root 'icon.svg') -Raw
$svg = $source.DocumentElement
$rect = $svg.SelectSingleNode("*[local-name()='rect']")
$gradient = [Windows.Media.LinearGradientBrush]::new()
$gradient.StartPoint = [Windows.Point]::new(0, 0)
$gradient.EndPoint = [Windows.Point]::new(1, 1)
foreach ($stop in $svg.SelectNodes("*[local-name()='defs']/*[local-name()='linearGradient']/*[local-name()='stop']")) {
    $gradient.GradientStops.Add([Windows.Media.GradientStop]::new(
        [Windows.Media.ColorConverter]::ConvertFromString($stop.GetAttribute('stop-color')),
        [double]::Parse($stop.GetAttribute('offset'), [Globalization.CultureInfo]::InvariantCulture)))
}
$gradient.Freeze()
function Render-Icon([int]$Size) {
    $visual = [Windows.Media.DrawingVisual]::new()
    $drawing = $visual.RenderOpen()
    $drawing.PushTransform([Windows.Media.ScaleTransform]::new($Size / 512.0, $Size / 512.0))
    $drawing.DrawRoundedRectangle($gradient, $null,
        [Windows.Rect]::new([double]$rect.x, [double]$rect.y, [double]$rect.width, [double]$rect.height), [double]$rect.rx, [double]$rect.rx)
    foreach ($path in $svg.SelectNodes("*[local-name()='path']")) {
        $brush = [Windows.Media.SolidColorBrush]::new([Windows.Media.ColorConverter]::ConvertFromString($path.stroke))
        $pen = [Windows.Media.Pen]::new($brush, [double]$path.GetAttribute('stroke-width'))
        $pen.StartLineCap = [Windows.Media.PenLineCap]::Round
        $pen.EndLineCap = [Windows.Media.PenLineCap]::Round
        $pen.LineJoin = [Windows.Media.PenLineJoin]::Round
        $drawing.DrawGeometry($null, $pen, [Windows.Media.Geometry]::Parse($path.d))
    }
    $drawing.Pop()
    $drawing.Close()
    $bitmap = [Windows.Media.Imaging.RenderTargetBitmap]::new($Size, $Size, 96, 96, [Windows.Media.PixelFormats]::Pbgra32)
    $bitmap.Render($visual)
    $encoder = [Windows.Media.Imaging.PngBitmapEncoder]::new()
    $encoder.Frames.Add([Windows.Media.Imaging.BitmapFrame]::Create($bitmap))
    $stream = [IO.MemoryStream]::new()
    try { $encoder.Save($stream); return ,$stream.ToArray() } finally { $stream.Dispose() }
}
[IO.File]::WriteAllBytes((Join-Path $root 'icon.png'), (Render-Icon 512))
$sizes = @(16, 24, 32, 48, 64, 128, 256)
$frames = @($sizes | ForEach-Object { ,(Render-Icon $_) })
$folder = Join-Path $root 'assets\branding'
New-Item -ItemType Directory -Force -Path $folder | Out-Null
$stream = [IO.File]::Create((Join-Path $folder 'moasearch.ico'))
$writer = [IO.BinaryWriter]::new($stream)
try {
    $writer.Write([uint16]0); $writer.Write([uint16]1); $writer.Write([uint16]$sizes.Count)
    $offset = 6 + 16 * $sizes.Count
    for ($i=0; $i -lt $sizes.Count; $i++) {
        $dimension = if ($sizes[$i] -eq 256) { 0 } else { $sizes[$i] }
        $writer.Write([byte]$dimension); $writer.Write([byte]$dimension)
        $writer.Write([byte]0); $writer.Write([byte]0)
        $writer.Write([uint16]1); $writer.Write([uint16]32)
        $writer.Write([uint32]$frames[$i].Length); $writer.Write([uint32]$offset)
        $offset += $frames[$i].Length
    }
    foreach ($frame in $frames) { $writer.Write([byte[]]$frame) }
} finally { $writer.Dispose(); $stream.Dispose() }
Write-Output 'Created icon.png and assets/branding/moasearch.ico.'
