#Requires -RunAsAdministrator
<#
生成「找回删除的文件」的对照镜像：分区和格式化都由 Windows 自己完成，写入已知内容的文件，再删掉一部分。
产出固定大小的 VHD（就是 raw 镜像末尾多 512 字节），以及每个镜像的清单 <镜像名>.tsv：
卷标、路径、长度、SHA-256、类别（live / live-fragmented / live-compressed / deleted / deleted-fragmented / deleted-shortname）。
NTFS 卷另有：常驻在 MFT 记录里的小文件、稀疏文件、硬链接、带备用数据流的文件、NTFS 压缩的文件，
以及运行表长到要用属性列表的稀疏文件（删除一个、保留一个）。

需要管理员权限（diskpart 建虚拟磁盘、挂载）。Windows PowerShell 5.1 按系统代码页读不带 BOM 的脚本，
所以按 UTF-8 读进来再执行：

    $script = Get-Content -Raw -Encoding UTF8 <仓库>\crates\fs\tests\make-test-images.ps1
    & ([ScriptBlock]::Create($script)) -OutDir <输出目录>

输出目录不要放在要做恢复测试的盘上；已有同名镜像时停止，不覆盖。
只生成其中几个镜像时用 -Only，例如 -Only mbr-ntfs,gpt-ntfs-64k。
#>
param(
    [Parameter(Mandatory = $true)]
    [string] $OutDir,
    [string[]] $Only
)

$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

# 中文名字用字符码拼出来，脚本本身只含 ASCII，避免被按错误的代码页读坏
$photoDir = [string]::new([char[]](0x7167, 0x7247))          # 照片
$travel = [string]::new([char[]](0x65C5, 0x884C))            # 旅行
$deletedCn = [string]::new([char[]](0x5DF2, 0x5220, 0x9664)) # 已删除

function New-Content([int] $Seed, [int] $Length) {
    $bytes = New-Object byte[] $Length
    (New-Object System.Random $Seed).NextBytes($bytes)
    return , $bytes
}

function Get-Sha256([byte[]] $Bytes) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        return ([System.BitConverter]::ToString($sha.ComputeHash($Bytes)) -replace '-', '').ToLowerInvariant()
    } finally {
        $sha.Dispose()
    }
}

# 写一个文件并记入清单；路径用 / 分隔，相对卷的根目录
function Write-TestFile($Manifest, [string] $Label, [string] $Root, [string] $Path, [int] $Seed, [int] $Length, [string] $Kind) {
    $full = Join-Path $Root ($Path -replace '/', '\')
    New-Item -ItemType Directory -Force -Path (Split-Path $full -Parent) | Out-Null
    $bytes = New-Content $Seed $Length
    [System.IO.File]::WriteAllBytes($full, $bytes)
    $Manifest.Add([pscustomobject]@{ Label = $Label; Path = $Path; Length = $Length; Sha = (Get-Sha256 $bytes); Kind = $Kind; Full = $full })
}

# 两个文件交替追加，让文件系统把它们的簇交错分配（碎片）
function Write-Interleaved($Manifest, [string] $Label, [string] $Root, [string] $PathA, [string] $PathB, [int] $Seed, [int] $Chunks, [string] $KindA, [string] $KindB) {
    $chunk = 64 * 1024
    $dataA = New-Content $Seed ($chunk * $Chunks)
    $dataB = New-Content ($Seed + 1) ($chunk * $Chunks)
    $fullA = Join-Path $Root ($PathA -replace '/', '\')
    $fullB = Join-Path $Root ($PathB -replace '/', '\')
    $a = [System.IO.File]::Open($fullA, 'CreateNew', 'Write')
    $b = [System.IO.File]::Open($fullB, 'CreateNew', 'Write')
    try {
        for ($i = 0; $i -lt $Chunks; $i++) {
            $a.Write($dataA, $i * $chunk, $chunk); $a.Flush($true)
            $b.Write($dataB, $i * $chunk, $chunk); $b.Flush($true)
        }
    } finally {
        $a.Dispose(); $b.Dispose()
    }
    $Manifest.Add([pscustomobject]@{ Label = $Label; Path = $PathA; Length = $dataA.Length; Sha = (Get-Sha256 $dataA); Kind = $KindA; Full = $fullA })
    $Manifest.Add([pscustomobject]@{ Label = $Label; Path = $PathB; Length = $dataB.Length; Sha = (Get-Sha256 $dataB); Kind = $KindB; Full = $fullB })
}

# NTFS 才有的情况；在删除之前写入
function Fill-NtfsExtras($Manifest, [string] $Label, [string] $Root, [int] $Seed) {
    # 小到内容常驻在 MFT 记录里的文件，现存的与删除的各一个
    Write-TestFile $Manifest $Label $Root 'tiny.txt' ($Seed + 30) 300 'live'
    Write-TestFile $Manifest $Label $Root 'deleted/tiny-deleted.txt' ($Seed + 31) 200 'deleted'

    # 稀疏文件：开头、结尾各 64 KiB 数据，中间 1 MiB 释放成空洞（读作 0）
    $head = New-Content ($Seed + 32) 65536
    $tail = New-Content ($Seed + 33) 65536
    $sparse = New-Object byte[] (65536 + 1048576 + 65536)
    [Array]::Copy($head, 0, $sparse, 0, 65536)
    [Array]::Copy($tail, 0, $sparse, 65536 + 1048576, 65536)
    $full = Join-Path $Root 'sparse.bin'
    [System.IO.File]::WriteAllBytes($full, $sparse)
    fsutil sparse setflag $full | Out-Null
    fsutil sparse setrange $full 65536 1048576 | Out-Null
    $Manifest.Add([pscustomobject]@{ Label = $Label; Path = 'sparse.bin'; Length = $sparse.Length; Sha = (Get-Sha256 $sparse); Kind = 'live'; Full = $full })

    # 硬链接：同一个文件的两个名字
    Write-TestFile $Manifest $Label $Root 'links/original.bin' ($Seed + 34) 20000 'live'
    $original = $Manifest[$Manifest.Count - 1]
    $linkFull = Join-Path $Root 'links\hardlink.bin'
    New-Item -ItemType HardLink -Path $linkFull -Target $original.Full | Out-Null
    $Manifest.Add([pscustomobject]@{ Label = $Label; Path = 'links/hardlink.bin'; Length = $original.Length; Sha = $original.Sha; Kind = 'live'; Full = $linkFull })

    # 备用数据流：主数据流照常对照
    Write-TestFile $Manifest $Label $Root 'with-stream.txt' ($Seed + 35) 4000 'live'
    Set-Content -LiteralPath $Manifest[$Manifest.Count - 1].Full -Stream 'extra' -Value 'alternate data stream'

    # NTFS 压缩（簇大于 4 KiB 的卷不支持压缩，压不了就按普通文件对照）
    Write-TestFile $Manifest $Label $Root 'packed.bin' ($Seed + 36) 200000 'live'
    $packed = $Manifest[$Manifest.Count - 1]
    compact /c /q $packed.Full | Out-Null
    if ((Get-Item -LiteralPath $packed.Full).Attributes -band [System.IO.FileAttributes]::Compressed) {
        $packed.Kind = 'live-compressed'
    }

    # 64 KiB 数据、64 KiB 空洞交替 600 次：运行表 1200 段，一条 MFT 记录放不下，要用属性列表
    Write-ManyRuns $Manifest $Label $Root 'listed-a.bin' ($Seed + 40) 'live-fragmented'
    Write-ManyRuns $Manifest $Label $Root 'listed-b.bin' ($Seed + 41) 'deleted-fragmented'
}

# 稀疏文件里每隔 64 KiB 写 64 KiB（64 KiB 是 4 KiB 簇卷上稀疏分配的单位，也是 64 KiB 簇卷的一簇）
function Write-ManyRuns($Manifest, [string] $Label, [string] $Root, [string] $Path, [int] $Seed, [string] $Kind) {
    $block = 65536
    $count = 600
    $content = New-Object byte[] ($block * 2 * $count)
    $full = Join-Path $Root $Path
    [System.IO.File]::WriteAllBytes($full, (New-Object byte[] 0))
    fsutil sparse setflag $full | Out-Null
    $stream = [System.IO.File]::Open($full, 'Open', 'Write')
    try {
        $stream.SetLength($content.Length)
        for ($i = 0; $i -lt $count; $i++) {
            $data = New-Content ($Seed * 1000 + $i) $block
            [Array]::Copy($data, 0, $content, $i * 2 * $block, $block)
            $stream.Position = $i * 2 * $block
            $stream.Write($data, 0, $block)
        }
        $stream.Flush($true)
    } finally {
        $stream.Dispose()
    }
    $Manifest.Add([pscustomobject]@{ Label = $Label; Path = $Path; Length = $content.Length; Sha = (Get-Sha256 $content); Kind = $Kind; Full = $full })
}

# 往挂在 $Root 的卷里写测试文件，再删掉标为 deleted 的；$Scale 控制大文件与文件数量
function Fill-Volume($Manifest, [string] $Label, [string] $Root, [int] $Scale, $Partition, [string] $Fs) {
    $seed = [Math]::Abs($Label.GetHashCode() % 100000)
    Write-TestFile $Manifest $Label $Root 'hello.txt' ($seed + 1) 1000 'live'
    Write-TestFile $Manifest $Label $Root "$photoDir/$travel 2024.jpg" ($seed + 2) 300000 'live'
    Write-TestFile $Manifest $Label $Root 'Documents/Reports/a long file name with spaces and more than 13 characters.docx' ($seed + 3) 50000 'live'
    Write-TestFile $Manifest $Label $Root 'big.bin' ($seed + 4) (1048576 * $Scale) 'live'
    $count = 50 * $Scale
    for ($i = 0; $i -lt $count; $i++) {
        Write-TestFile $Manifest $Label $Root ('many/f{0:D4}.bin' -f $i) ($seed + 100 + $i) (100 + ($i * 37) % 3000) 'live'
    }
    Write-Interleaved $Manifest $Label $Root 'frag-a.bin' 'frag-b.bin' ($seed + 10) 8 'live-fragmented' 'deleted-fragmented'

    Write-TestFile $Manifest $Label $Root 'deleted/photo-deleted.jpg' ($seed + 20) 200000 'deleted'
    Write-TestFile $Manifest $Label $Root "deleted/notes $deletedCn.txt" ($seed + 21) 5000 'deleted'
    Write-TestFile $Manifest $Label $Root 'OLDNAME.TXT' ($seed + 22) 3000 'deleted-shortname'
    Write-TestFile $Manifest $Label $Root 'trash/inner.bin' ($seed + 23) 40000 'deleted'
    Write-TestFile $Manifest $Label $Root 'trash/sub/deep.txt' ($seed + 24) 700 'deleted'
    if ($Fs -eq 'ntfs') {
        Fill-NtfsExtras $Manifest $Label $Root $seed
    }

    # 先把数据刷到盘上再删：刚写完就删的文件，数据可能根本没离开写缓存，盘上什么都找不到
    $Partition | Get-Volume | Write-VolumeCache
    foreach ($item in @($Manifest | Where-Object { $_.Label -eq $Label -and $_.Kind -like 'deleted*' })) {
        Remove-Item -LiteralPath $item.Full -Force
    }
    Remove-Item -LiteralPath (Join-Path $Root 'trash') -Recurse -Force
    $Partition | Get-Volume | Write-VolumeCache
}

function New-TestImage([string] $Name, [int] $SizeMb, [string] $Style, $Volumes) {
    if ($Only -and $Only -notcontains $Name) {
        return
    }
    $vhd = Join-Path $OutDir "$Name.vhd"
    if (Test-Path -LiteralPath $vhd) {
        throw "$vhd already exists"
    }
    $lines = @("create vdisk file=""$vhd"" maximum=$SizeMb type=fixed", "select vdisk file=""$vhd""", 'attach vdisk', "convert $Style")
    foreach ($volume in $Volumes) {
        if ($volume.Logical) {
            $lines += 'create partition extended'
            $lines += "create partition logical size=$($volume.SizeMb)"
        } else {
            $lines += "create partition primary size=$($volume.SizeMb)"
        }
        $unit = if ($volume.Unit) { " unit=$($volume.Unit)" } else { '' }
        $lines += "format fs=$($volume.Fs) quick label=$($volume.Label)$unit"
    }
    $commands = Join-Path $OutDir "$Name.diskpart.txt"
    [System.IO.File]::WriteAllLines($commands, $lines)
    diskpart /s $commands | ForEach-Object { Write-Host $_ }
    if ($LASTEXITCODE -ne 0) {
        throw "diskpart failed: $LASTEXITCODE"
    }

    # 不分配盘符（盘符可能不够、也可能分配失败）：每个分区挂到输出目录下的一个空文件夹，写完再取下
    $manifest = New-Object System.Collections.Generic.List[object]
    try {
        $disk = Get-DiskImage -ImagePath $vhd | Get-Disk
        foreach ($volume in $Volumes) {
            $partition = Get-Partition -DiskNumber $disk.Number |
                Where-Object { ($_ | Get-Volume -ErrorAction SilentlyContinue).FileSystemLabel -eq $volume.Label }
            if ($null -eq $partition) {
                throw "volume $($volume.Label) not found"
            }
            $mount = Join-Path $OutDir "mnt-$Name-$($volume.Label)"
            New-Item -ItemType Directory -Force -Path $mount | Out-Null
            $partition | Add-PartitionAccessPath -AccessPath "$mount\"
            try {
                Fill-Volume $manifest $volume.Label "$mount\" $volume.Scale $partition $volume.Fs
            } finally {
                $partition | Remove-PartitionAccessPath -AccessPath "$mount\"
                Remove-Item -LiteralPath $mount
            }
        }
    } finally {
        Dismount-DiskImage -ImagePath $vhd | Out-Null
    }
    $tsv = $manifest | ForEach-Object { "{0}`t{1}`t{2}`t{3}`t{4}" -f $_.Label, $_.Path, $_.Length, $_.Sha, $_.Kind }
    [System.IO.File]::WriteAllLines((Join-Path $OutDir "$Name.tsv"), [string[]]$tsv, (New-Object System.Text.UTF8Encoding $false))
    Remove-Item -LiteralPath $commands
    Write-Host "$vhd : $($manifest.Count) files"
}

New-TestImage 'mbr-fat' 320 'mbr' @(
    @{ Label = 'FAT32TEST'; SizeMb = 128; Fs = 'fat32'; Scale = 20; Logical = $false },
    @{ Label = 'EXFATTEST'; SizeMb = 100; Fs = 'exfat'; Scale = 20; Logical = $false },
    @{ Label = 'FAT16TEST'; SizeMb = 40; Fs = 'fat'; Scale = 4; Logical = $false },
    @{ Label = 'FAT12TEST'; SizeMb = 12; Fs = 'fat'; Scale = 1; Logical = $true }
)
New-TestImage 'gpt-mixed' 200 'gpt' @(
    @{ Label = 'GPTEXFAT'; SizeMb = 80; Fs = 'exfat'; Scale = 8; Logical = $false },
    @{ Label = 'GPTFAT32'; SizeMb = 100; Fs = 'fat32'; Scale = 8; Logical = $false }
)
New-TestImage 'mbr-ntfs' 300 'mbr' @(
    @{ Label = 'NTFSTEST'; SizeMb = 260; Fs = 'ntfs'; Scale = 20; Logical = $false }
)
New-TestImage 'gpt-ntfs-64k' 300 'gpt' @(
    @{ Label = 'NTFS64K'; SizeMb = 260; Fs = 'ntfs'; Scale = 8; Logical = $false; Unit = '64K' }
)
