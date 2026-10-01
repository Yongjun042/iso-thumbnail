<#
.SYNOPSIS
  Builds a set of small .iso test images that mimic Blu-ray and DVD-Video disc layouts.

.DESCRIPTION
  Uses the IMAPI2 file-system image API that ships with Windows (no admin
  rights needed) to write UDF 1.02 / 2.01 / 2.50, ISO 9660 and Joliet images.
  The generated JPEG/PNG artwork is drawn with System.Drawing.

  When ffmpeg is on PATH, DVD-Video images are built too: real MPEG-2 program
  streams from ffmpeg's DVD muxer (VIDEO_TS with a main title split into two
  VOBs, a shorter second title set and a menu), a jacket picture variant and a
  PAL 16:9 variant. Without ffmpeg the DVD images are skipped with a warning.

.PARAMETER OutDir
  Directory that receives the .iso files (created if missing).
#>
param(
    [Parameter(Mandatory = $true)]
    [string] $OutDir
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$writerSource = @"
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;

public static class IsoWriter
{
    public static void Write(string path, object streamObject, int blockSize, int totalBlocks)
    {
        IStream stream = (IStream)streamObject;
        byte[] buffer = new byte[blockSize];
        IntPtr bytesRead = Marshal.AllocHGlobal(4);
        try
        {
            using (FileStream file = new FileStream(path, FileMode.Create, FileAccess.Write))
            {
                for (int i = 0; i < totalBlocks; i++)
                {
                    stream.Read(buffer, blockSize, bytesRead);
                    int n = Marshal.ReadInt32(bytesRead);
                    if (n <= 0) { break; }
                    file.Write(buffer, 0, n);
                }
            }
        }
        finally
        {
            Marshal.FreeHGlobal(bytesRead);
        }
    }
}
"@
if (-not ([System.Management.Automation.PSTypeName]'IsoWriter').Type) {
    Add-Type -TypeDefinition $writerSource
}

function New-TestPicture {
    param([string] $Path, [int] $Width, [int] $Height, [string] $Label, [string] $Format = 'Jpeg')
    $bmp = New-Object System.Drawing.Bitmap $Width, $Height
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $rect = New-Object System.Drawing.Rectangle 0, 0, $Width, $Height
    $brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush $rect, ([System.Drawing.Color]::Navy), ([System.Drawing.Color]::Orange), 45
    if ($Format -eq 'Png') {
        $g.Clear([System.Drawing.Color]::Transparent)
        $g.FillEllipse($brush, $rect)
    } else {
        $g.FillRectangle($brush, $rect)
    }
    $font = New-Object System.Drawing.Font 'Arial', ([Math]::Max(8, [int]($Height / 8))), ([System.Drawing.FontStyle]::Bold)
    $g.DrawString($Label, $font, [System.Drawing.Brushes]::White, 12, 12)
    $g.Dispose()
    $bmp.Save($Path, [System.Drawing.Imaging.ImageFormat]::$Format)
    $bmp.Dispose()
}

function New-TextFile {
    param([string] $Path, [string] $Content)
    $dir = Split-Path -Parent $Path
    if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir | Out-Null }
    [System.IO.File]::WriteAllText($Path, $Content)
}

function New-Tree {
    param([string] $Root, [switch] $ShortNames, [string] $Artwork = 'DL')
    if (Test-Path $Root) { Remove-Item -Recurse -Force $Root }
    New-Item -ItemType Directory -Path $Root | Out-Null
    $bdmv = Join-Path $Root 'BDMV'
    $idx = if ($ShortNames) { 'INDEX.BDM' } else { 'index.bdmv' }
    New-TextFile (Join-Path $bdmv $idx) 'INDX0200'
    $mobj = if ($ShortNames) { 'MOVIEOBJ.BDM' } else { 'MovieObject.bdmv' }
    New-TextFile (Join-Path $bdmv $mobj) 'MOBJ0200'
    New-TextFile (Join-Path $Root 'CERTIFICATE\id.bdmv') 'BDID0200'
    # Enough files to push the directory past one 2 KiB block.
    $streamExt = if ($ShortNames) { 'M2T' } else { 'm2ts' }
    for ($i = 0; $i -lt 120; $i++) {
        New-TextFile (Join-Path $bdmv ('STREAM\{0:D5}.{1}' -f $i, $streamExt)) ('stream ' + $i)
    }
    New-TextFile (Join-Path $bdmv ('PLAYLIST\00000.' + $(if ($ShortNames) { 'MPL' } else { 'mpls' }))) 'MPLS0200'
    if ($Artwork -eq 'DL') {
        $dl = Join-Path $bdmv 'META\DL'
        New-Item -ItemType Directory -Path $dl | Out-Null
        New-TextFile (Join-Path $dl $(if ($ShortNames) { 'BDMT_ENG.XML' } else { 'bdmt_eng.xml' })) '<?xml version="1.0"?><disclib><di:title><di:name>Test Disc</di:name></di:title></disclib>'
        if ($ShortNames) {
            New-TestPicture (Join-Path $dl 'COVER1.JPG') 416 240 'small 416x240'
            New-TestPicture (Join-Path $dl 'COVER2.JPG') 640 360 'large 640x360'
        } else {
            New-TestPicture (Join-Path $dl 'TESTDISC_416x240.jpg') 416 240 'small 416x240'
            New-TestPicture (Join-Path $dl 'TESTDISC_640x360.jpg') 640 360 'large 640x360'
        }
    } elseif ($Artwork -eq 'TN') {
        $tn = Join-Path $bdmv 'META\TN'
        New-Item -ItemType Directory -Path $tn | Out-Null
        New-TextFile (Join-Path $tn 'tnmt_eng_00001.xml') '<?xml version="1.0"?><tnmt/>'
        New-TestPicture (Join-Path $tn 'TRACK_416x240.jpg') 416 240 'track 416x240'
    }
}

function New-TestImage {
    param([string] $Out, [string] $Source, [int] $FileSystems, [int] $UdfRevision, [string] $VolumeName)
    $fsi = New-Object -ComObject IMAPI2FS.MsftFileSystemImage
    $fsi.FileSystemsToCreate = $FileSystems
    if ($FileSystems -band 4) { $fsi.UDFRevision = $UdfRevision }
    $fsi.VolumeName = $VolumeName
    $fsi.Root.AddTree($Source, $false)
    $result = $fsi.CreateResultImage()
    [IsoWriter]::Write($Out, $result.ImageStream, $result.BlockSize, $result.TotalBlocks)
    '{0,-24} {1,10} bytes  fs={2} udf=0x{3:X}' -f (Split-Path $Out -Leaf), (Get-Item $Out).Length, $FileSystems, $UdfRevision
}

if (-not (Test-Path $OutDir)) { New-Item -ItemType Directory -Path $OutDir | Out-Null }
$OutDir = (Resolve-Path $OutDir).Path
$work = Join-Path $OutDir 'trees'
if (-not (Test-Path $work)) { New-Item -ItemType Directory -Path $work | Out-Null }

$treeLong = Join-Path $work 'bd_long'
$treeShort = Join-Path $work 'bd_short'
$treeTn = Join-Path $work 'bd_tn'
$treeNone = Join-Path $work 'bd_none'
$treeRoot = Join-Path $work 'root_cover'

New-Tree -Root $treeLong
New-Tree -Root $treeShort -ShortNames
New-Tree -Root $treeTn -Artwork 'TN'
New-Tree -Root $treeNone -Artwork 'none'
if (Test-Path $treeRoot) { Remove-Item -Recurse -Force $treeRoot }
New-Item -ItemType Directory -Path $treeRoot | Out-Null
New-TestPicture (Join-Path $treeRoot 'cover.png') 300 300 'root cover' 'Png'
New-TextFile (Join-Path $treeRoot 'readme.txt') 'plain data disc with cover art'

# FileSystemsToCreate: 1 = ISO 9660, 2 = Joliet, 4 = UDF
New-TestImage (Join-Path $OutDir 'bd_udf250.iso')       $treeLong  4 0x250 'BD_UDF250'
New-TestImage (Join-Path $OutDir 'bd_udf201.iso')       $treeLong  4 0x201 'BD_UDF201'
New-TestImage (Join-Path $OutDir 'bd_udf102.iso')       $treeLong  4 0x102 'BD_UDF102'
New-TestImage (Join-Path $OutDir 'bd_bridge.iso')       $treeLong  7 0x250 'BD_BRIDGE'
New-TestImage (Join-Path $OutDir 'bd_joliet.iso')       $treeLong  3 0     'BD_JOLIET'
New-TestImage (Join-Path $OutDir 'bd_iso9660.iso')      $treeShort 1 0     'BD_ISO9660'
New-TestImage (Join-Path $OutDir 'bd_tn_udf250.iso')    $treeTn    4 0x250 'BD_TN'
New-TestImage (Join-Path $OutDir 'bd_nothumb_udf250.iso') $treeNone 4 0x250 'BD_NOTHUMB'
New-TestImage (Join-Path $OutDir 'rootcover_udf250.iso') $treeRoot 4 0x250 'ROOT_COVER'
New-TestImage (Join-Path $OutDir 'rootcover_joliet.iso') $treeRoot 3 0     'ROOT_COVER_J'

# ---------------------------------------------------------------------------
# DVD-Video images (need ffmpeg)
# ---------------------------------------------------------------------------

$ffmpeg = (Get-Command ffmpeg -ErrorAction SilentlyContinue)
if (-not $ffmpeg) {
    Write-Warning 'ffmpeg is not on PATH: DVD-Video test images skipped.'
    return
}
$ffmpeg = $ffmpeg.Source

function Invoke-Ffmpeg {
    param([string[]] $Arguments)
    & $ffmpeg -hide_banner -loglevel error -nostdin -y @Arguments
    if ($LASTEXITCODE -ne 0) { throw "ffmpeg failed: $($Arguments -join ' ')" }
}

function New-DvdVob {
    # A DVD-compliant program stream made of lavfi sections (@(source, seconds[,
    # extra filters])), optionally letterboxed: a 16:9 picture with black bars inside the 4:3 frame.
    param([string] $Out, [string] $Target, [string] $Aspect, [object[]] $Sections, [switch] $Letterbox)
    $pal = $Target -like 'pal*'
    $size = if ($pal) { '720x576' } else { '720x480' }
    $rate = if ($pal) { '25' } else { '30000/1001' }
    $inputs = @()
    $chains = @()
    $labels = ''
    $total = 0
    for ($i = 0; $i -lt $Sections.Count; $i++) {
        $source = $Sections[$i][0]
        $seconds = $Sections[$i][1]
        $total += $seconds
        $sep = if ($source.Contains('=')) { ':' } else { '=' }
        $inputs += @('-f', 'lavfi', '-t', "$seconds", '-i', "${source}${sep}s=${size}:r=$rate")
        $box = ''
        if ($Letterbox) {
            $box = if ($pal) { ',scale=720:432,pad=720:576:0:72:black' } else { ',scale=720:360,pad=720:480:0:60:black' }
        }
        $extra = if ($Sections[$i].Count -gt 2) { ',' + $Sections[$i][2] } else { '' }
        $chains += "[${i}:v]format=yuv420p$extra$box,setsar=1[v$i]"
        $labels += "[v$i]"
    }
    $audio = $Sections.Count
    $inputs += @('-f', 'lavfi', '-t', "$total", '-i', 'sine=frequency=440:sample_rate=48000')
    $graph = ($chains -join ';') + ";${labels}concat=n=$($Sections.Count):v=1:a=0[v]"
    Invoke-Ffmpeg ($inputs + @('-filter_complex', $graph, '-map', '[v]', '-map', "${audio}:a",
        '-target', $Target, '-aspect', $Aspect, $Out))
}

function Split-Vob {
    # Splits a VOB into two parts at a pack (2048-byte) boundary near the middle,
    # like the 1 GiB parts of a real title.
    param([string] $Path, [string] $First, [string] $Second)
    $bytes = [System.IO.File]::ReadAllBytes($Path)
    $cut = [int]([Math]::Floor($bytes.Length / 2 / 2048) * 2048)
    [System.IO.File]::WriteAllBytes($First, $bytes[0..($cut - 1)])
    [System.IO.File]::WriteAllBytes($Second, $bytes[$cut..($bytes.Length - 1)])
    Remove-Item $Path
}

function New-IfoStub {
    # Placeholder IFO/BUP files: the handler does not read them.
    param([string] $Path, [string] $Kind)
    $b = New-Object byte[] 2048
    $id = [System.Text.Encoding]::ASCII.GetBytes("DVDVIDEO-$Kind")
    [Array]::Copy($id, $b, $id.Length)
    [System.IO.File]::WriteAllBytes($Path, $b)
}

function New-JacketPicture {
    # A jacket picture: one MPEG-2 I-picture as an elementary stream.
    param([string] $Out, [string] $Size)
    $photo = Join-Path $env:WINDIR 'Web\4K\Wallpaper\Windows\img0_1920x1200.jpg'
    $source = if (Test-Path $photo) { @('-i', $photo) } else { @('-f', 'lavfi', '-i', 'testsrc2=s=1280x720') }
    Invoke-Ffmpeg ($source + @('-vf', "scale=$Size,format=yuv420p", '-frames:v', '1', '-c:v', 'mpeg2video',
        '-q:v', '2', '-aspect', '4:3', '-f', 'mpeg2video', $Out))
}

function New-DvdTree {
    param([string] $Root, [string] $Target = 'ntsc-dvd', [string] $Aspect = '4:3', [switch] $Letterbox, [switch] $Jacket)
    if (Test-Path $Root) { Remove-Item -Recurse -Force $Root }
    $vts = Join-Path $Root 'VIDEO_TS'
    New-Item -ItemType Directory -Path $vts | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $Root 'AUDIO_TS') | Out-Null
    foreach ($n in @('VIDEO_TS.IFO', 'VIDEO_TS.BUP')) { New-IfoStub (Join-Path $vts $n) 'VMG' }
    foreach ($n in @('VTS_01_0.IFO', 'VTS_01_0.BUP', 'VTS_02_0.IFO', 'VTS_02_0.BUP')) { New-IfoStub (Join-Path $vts $n) 'VTS' }
    # Menu; main title split in two parts; a short second title. The main title
    # is bright, then near-black around the first sampling point (25 % of its
    # bytes: the faint noise keeps the dark part from compressing to nothing),
    # then bright again.
    New-DvdVob (Join-Path $vts 'VIDEO_TS.VOB') $Target $Aspect @(, @('smptebars', 2))
    $main = Join-Path $vts 'main.vob'
    New-DvdVob $main $Target $Aspect @(@('mandelbrot', 3), @('color=c=black', 3, 'noise=alls=10:allf=t'), @('testsrc2', 13)) -Letterbox:$Letterbox
    Split-Vob $main (Join-Path $vts 'VTS_01_1.VOB') (Join-Path $vts 'VTS_01_2.VOB')
    New-DvdVob (Join-Path $vts 'VTS_02_1.VOB') $Target $Aspect @(, @('rgbtestsrc', 4))
    if ($Jacket) {
        $jp = Join-Path $Root 'JACKET_P'
        New-Item -ItemType Directory -Path $jp | Out-Null
        New-JacketPicture (Join-Path $jp 'J00___5L.MP2') '720:480'
        New-JacketPicture (Join-Path $jp 'J00___5M.MP2') '176:112'
        New-JacketPicture (Join-Path $jp 'J00___5S.MP2') '96:64'
    }
}

$dvdNtsc = Join-Path $work 'dvd_ntsc'
$dvdJacket = Join-Path $work 'dvd_jacket'
$dvdPal = Join-Path $work 'dvd_pal'
New-DvdTree -Root $dvdNtsc -Letterbox
New-DvdTree -Root $dvdJacket -Jacket
New-DvdTree -Root $dvdPal -Target 'pal-dvd' -Aspect '16:9'

# DVD-Video discs carry UDF 1.02 and ISO 9660 side by side (FileSystemsToCreate 5).
New-TestImage (Join-Path $OutDir 'dvd_bridge.iso')  $dvdNtsc   5 0x102 'DVD_BRIDGE'
New-TestImage (Join-Path $OutDir 'dvd_udf102.iso')  $dvdNtsc   4 0x102 'DVD_UDF102'
New-TestImage (Join-Path $OutDir 'dvd_iso9660.iso') $dvdNtsc   1 0     'DVD_ISO9660'
New-TestImage (Join-Path $OutDir 'dvd_jacket.iso')  $dvdJacket 5 0x102 'DVD_JACKET'
New-TestImage (Join-Path $OutDir 'dvd_pal169.iso')  $dvdPal    5 0x102 'DVD_PAL169'
