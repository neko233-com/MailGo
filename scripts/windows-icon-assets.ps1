Set-StrictMode -Version Latest

function Open-MailGoSourceIcon([string]$SourcePath) {
    Add-Type -AssemblyName System.Drawing
    $resolved = (Resolve-Path -LiteralPath $SourcePath).Path
    $image = [System.Drawing.Bitmap]::FromFile($resolved)
    if ($image.Width -ne $image.Height -or $image.Width -lt 600) {
        $image.Dispose()
        throw 'MailGo icon source must be a square PNG of at least 600x600 pixels'
    }
    return $image
}

function Save-MailGoIconPng(
    [System.Drawing.Image]$Source,
    [ValidateRange(16, 1024)][int]$Size,
    [string]$Path
) {
    $parent = Split-Path -Parent $Path
    if ($parent) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }

    $bitmap = [System.Drawing.Bitmap]::new(
        $Size,
        $Size,
        [System.Drawing.Imaging.PixelFormat]::Format32bppArgb
    )
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $attributes = [System.Drawing.Imaging.ImageAttributes]::new()
    try {
        $graphics.Clear([System.Drawing.Color]::Transparent)
        $graphics.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
        $graphics.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
        $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
        $graphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
        $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
        $attributes.SetWrapMode([System.Drawing.Drawing2D.WrapMode]::TileFlipXY)
        $destination = [System.Drawing.Rectangle]::new(0, 0, $Size, $Size)
        $graphics.DrawImage(
            $Source,
            $destination,
            0,
            0,
            $Source.Width,
            $Source.Height,
            [System.Drawing.GraphicsUnit]::Pixel,
            $attributes
        )
        $bitmap.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
    } finally {
        $attributes.Dispose()
        $graphics.Dispose()
        $bitmap.Dispose()
    }
}

function Write-MailGoPngIco([string[]]$PngPaths, [string]$DestinationPath) {
    if ($PngPaths.Count -eq 0 -or $PngPaths.Count -gt [uint16]::MaxValue) {
        throw 'ICO generation requires between 1 and 65535 PNG images'
    }

    $images = foreach ($path in $PngPaths) {
        $bytes = [System.IO.File]::ReadAllBytes($path)
        if ($bytes.Length -lt 24 -or
            $bytes[0] -ne 0x89 -or $bytes[1] -ne 0x50 -or $bytes[2] -ne 0x4e -or $bytes[3] -ne 0x47 -or
            $bytes[12] -ne 0x49 -or $bytes[13] -ne 0x48 -or $bytes[14] -ne 0x44 -or $bytes[15] -ne 0x52) {
            throw "ICO source is not a valid PNG: $path"
        }
        $width = ([int]$bytes[16] -shl 24) -bor ([int]$bytes[17] -shl 16) -bor ([int]$bytes[18] -shl 8) -bor [int]$bytes[19]
        $height = ([int]$bytes[20] -shl 24) -bor ([int]$bytes[21] -shl 16) -bor ([int]$bytes[22] -shl 8) -bor [int]$bytes[23]
        if ($width -ne $height -or $width -lt 1 -or $width -gt 256) {
            throw "ICO PNG must be square and at most 256x256: $path"
        }
        [pscustomobject]@{ Size = $width; Bytes = $bytes }
    }
    $images = @($images | Sort-Object Size)

    $stream = [System.IO.MemoryStream]::new()
    $writer = [System.IO.BinaryWriter]::new($stream)
    try {
        $writer.Write([uint16]0)
        $writer.Write([uint16]1)
        $writer.Write([uint16]$images.Count)
        $offset = 6 + (16 * $images.Count)
        foreach ($image in $images) {
            $dimension = if ($image.Size -eq 256) { [byte]0 } else { [byte]$image.Size }
            $writer.Write($dimension)
            $writer.Write($dimension)
            $writer.Write([byte]0)
            $writer.Write([byte]0)
            $writer.Write([uint16]1)
            $writer.Write([uint16]32)
            $writer.Write([uint32]$image.Bytes.Length)
            $writer.Write([uint32]$offset)
            $offset += $image.Bytes.Length
        }
        foreach ($image in $images) { $writer.Write($image.Bytes) }
        $writer.Flush()
        [System.IO.File]::WriteAllBytes($DestinationPath, $stream.ToArray())
    } finally {
        $writer.Dispose()
        $stream.Dispose()
    }
}

function New-MailGoCoreIconSet([string]$SourcePath, [string]$DestinationDirectory) {
    $sizes = @(16, 20, 24, 30, 32, 36, 40, 48, 60, 64, 72, 80, 96, 128, 256)
    New-Item -ItemType Directory -Force -Path $DestinationDirectory | Out-Null
    $source = Open-MailGoSourceIcon $SourcePath
    try {
        $pngPaths = foreach ($size in $sizes) {
            $path = Join-Path $DestinationDirectory "mailgo-$size.png"
            Save-MailGoIconPng $source $size $path
            $path
        }
        Write-MailGoPngIco @($pngPaths) (Join-Path $DestinationDirectory 'mailgo.ico')
    } finally {
        $source.Dispose()
    }
}

function New-MailGoMsixAssets([string]$SourcePath, [string]$DestinationDirectory) {
    New-Item -ItemType Directory -Force -Path $DestinationDirectory | Out-Null
    $source = Open-MailGoSourceIcon $SourcePath
    try {
        foreach ($asset in @(
            @{ Name = 'StoreLogo.png'; Size = 50 },
            @{ Name = 'Square44x44Logo.png'; Size = 44 },
            @{ Name = 'Square150x150Logo.png'; Size = 150 }
        )) {
            Save-MailGoIconPng $source $asset.Size (Join-Path $DestinationDirectory $asset.Name)
        }

        foreach ($specification in @(
            @{ Stem = 'StoreLogo'; Base = 50 },
            @{ Stem = 'Square44x44Logo'; Base = 44 },
            @{ Stem = 'Square150x150Logo'; Base = 150 }
        )) {
            foreach ($scale in 100, 200, 400) {
                $size = [int]($specification.Base * $scale / 100)
                Save-MailGoIconPng $source $size (Join-Path $DestinationDirectory "$($specification.Stem).scale-$scale.png")
            }
        }

        foreach ($size in 16, 20, 24, 30, 32, 36, 40, 48, 60, 64, 72, 80, 96, 256) {
            $baseName = "Square44x44Logo.targetsize-$size"
            $defaultPath = Join-Path $DestinationDirectory "$baseName.png"
            Save-MailGoIconPng $source $size $defaultPath
            Copy-Item -LiteralPath $defaultPath -Destination (Join-Path $DestinationDirectory "${baseName}_altform-unplated.png") -Force
            Copy-Item -LiteralPath $defaultPath -Destination (Join-Path $DestinationDirectory "${baseName}_altform-lightunplated.png") -Force
        }
    } finally {
        $source.Dispose()
    }
}
