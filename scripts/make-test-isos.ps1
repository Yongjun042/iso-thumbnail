<#
.SYNOPSIS
  Builds a set of small .iso test images that mimic Blu-ray disc layouts.

.DESCRIPTION
  Uses the IMAPI2 file-system image API that ships with Windows (no admin
  rights needed) to write UDF 1.02 / 2.01 / 2.50, ISO 9660 and Joliet images.
  The generated JPEG/PNG artwork is drawn with System.Drawing.

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
