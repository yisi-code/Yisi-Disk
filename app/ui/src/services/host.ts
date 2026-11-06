// 与宿主通信的唯一入口：组件不直接调用 Tauri 命令
import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { open, save } from '@tauri-apps/plugin-dialog'

export interface AppInfo {
  version: string
}

export interface EngineCapabilities {
  version: string
  methods: string[]
}

/** 宿主返回的错误：code 交给界面翻译，detail 是附带的技术细节 */
export interface HostError {
  code: string
  detail: string
}

export type DiskBus =
  | 'ata'
  | 'sata'
  | 'scsi'
  | 'sas'
  | 'nvme'
  | 'usb'
  | 'sd'
  | 'mmc'
  | 'virtual'
  | 'other'

export interface DiskSummary {
  number: number
  path: string
  size: number
  sectorSize: number
  vendor: string
  model: string
  serial: string
  bus: DiskBus
  removable: boolean
  /** 认盘用的标识（厂商、型号、序列号），重新插拔、盘号变了也不变；与做镜像时记下的来源相同 */
  identity: string
  /** 固态盘；设备不回答时（常见于 USB 转接）为 null */
  solidState: boolean | null
  /** 系统认出的分区表格式；查不到时为 null */
  partitionStyle: 'mbr' | 'gpt' | 'raw' | null
  /** 这块盘上挂载在盘符上的卷，按盘符排序 */
  volumes: DiskVolume[]
}

/** 挂载在盘符上的卷（向系统查询，不读盘） */
export interface DiskVolume {
  letter: string
  /** 认卷用的标识（卷的 GUID 名），换了盘符、重新插拔也不变；与以这个卷为源做镜像时记下的来源相同 */
  identity: string
  label: string
  fileSystem: string
  totalBytes: number
  freeBytes: number
}

/** 任务的源：镜像文件（raw 或 E01），或按编号指定的物理磁盘、按盘符指定的卷 */
export type JobTarget =
  | { kind: 'image'; path: string }
  | { kind: 'disk'; diskNumber: number }
  | { kind: 'volume'; letter: string }

/** 物理磁盘或卷：读它要管理员权限，系统也可能正在往里写 */
export function isDeviceKind(kind: JobTarget['kind'] | null | undefined): boolean {
  return kind === 'disk' || kind === 'volume'
}

/**
 * 看路径像不像 E01 镜像的第一段（扩展名 .E01，不分大小写）：界面据此显示「E01 镜像」、不给写盘的按钮。
 * 引擎与宿主按文件开头的签名认，改了扩展名的 E01 照样只读，写盘时由宿主拒绝（writeTargetEwf）
 */
export function isEwfPath(path: string): boolean {
  return /\.e01$/i.test(path.trim())
}

/** 不能为这个源生成写盘计划的原因：卷（volume）、E01 镜像（ewf） */
export type WriteBlock = 'volume' | 'ewf'

/**
 * 为什么不能为这个源生成写盘计划；能写时为 null。raw 镜像文件与整块磁盘拿到的偏移就是盘上的偏移；
 * 卷上的偏移从卷首算起，换算到盘上要另做，所以卷不写；E01 是压缩的只读镜像，不写
 */
export function writePlanBlock(target: JobTarget): WriteBlock | null {
  switch (target.kind) {
    case 'image':
      return isEwfPath(target.path) ? 'ewf' : null
    case 'disk':
      return null
    case 'volume':
      return 'volume'
  }
}

/** 能不能为这个源生成写盘计划（原因见 writePlanBlock） */
export function canWritePlan(target: JobTarget | null): boolean {
  return target !== null && writePlanBlock(target) === null
}

/** 两个源是不是同一个 */
export function sameTarget(a: JobTarget | null, b: JobTarget | null): boolean {
  if (a === null || b === null || a.kind !== b.kind) {
    return a === b
  }
  switch (a.kind) {
    case 'image':
      return b.kind === 'image' && a.path === b.path
    case 'disk':
      return b.kind === 'disk' && a.diskNumber === b.diskNumber
    case 'volume':
      return b.kind === 'volume' && a.letter === b.letter
  }
}

/** 深度恢复支持的一种格式；扩展名同时是格式的标识 */
export interface FileType {
  extension: string
  /** 按内容可能改用的扩展名（ZIP 里是 Word 文档时存成 docx）；找到的文件的扩展名可能是其中之一 */
  variants: string[]
  /** 英文简述：界面按扩展名取本地化文案，取不到时显示它 */
  description: string
  maxSize: number
  enabledByDefault: boolean
}

export interface TypeTotal {
  count: number
  bytes: number
}

/** 深度恢复的结果 */
export interface CarveSummary {
  /** 实际存放结果的目录；只扫描时为 null */
  outputDir: string | null
  /** 找到的文件数与字节数，含不完整的 */
  files: number
  bytes: number
  /** 其中不完整的文件数（部分恢复）；旧引擎没有这一项 */
  incomplete?: number
  /** 按扩展名统计 */
  byType: Record<string, TypeTotal>
  unreadableBytes: number
  elapsedMs: number
}

/** 不完整的文件（部分恢复）在哪里断了：源的末尾、读不出的块、被别的文件覆盖、结构对不上 */
export type IncompleteReason = 'sourceEnd' | 'unreadable' | 'overwritten' | 'broken'

/** 深度恢复找到的一个文件 */
export interface FoundFile {
  name: string
  extension: string
  offset: number
  length: number
  /** 部分恢复存下的不完整文件：在哪里断了；完整的文件没有这一项 */
  incomplete?: IncompleteReason
  /** 由两段拼成的文件（两段重组）：各段在源上的位置，按文件里的先后；连续存放的文件没有这一项 */
  fragments?: FileFragment[]
}

/** 由几段拼成的文件里的一段：在源上的起点与长度 */
export interface FileFragment {
  offset: number
  length: number
}

/** 引擎生成的缩略图：图片在引擎里解码、缩小后重新编码成 PNG，界面不直接渲染恢复出的文件 */
export interface ImagePreview {
  /** 原图的尺寸 */
  width: number
  height: number
  /** PNG，Base64 */
  thumbnail: string
}

/** 恢复出的一个文件的预览 */
export interface FilePreview {
  length: number
  /** 文件开头最多 4096 字节，Base64，供十六进制视图使用 */
  head: string
  /** 内容是 JPEG 或 PNG 且能解码时才有 */
  image: ImagePreview | null
  /** 内容是 JPEG 或 PNG 但解码失败时的原因 */
  imageError: string | null
  /** NTFS 文件的备用数据流（有名字的数据流），其他情况为空 */
  streams: StreamPreview[]
}

export interface StreamPreview {
  name: string
  length: number
  /** 开头最多 1024 字节，Base64；读不出（加密、压缩方式不支持）时为空 */
  head: string
}

/** 打开卷时用哪一份引导扇区 */
/** 打开卷时用哪一份引导扇区；rebuilt 是主备都坏了时按分区里的结构推算出的（只在内存里换上，不写盘） */
export type BootCopy = 'primary' | 'backup' | 'rebuilt'

/** 引导扇区诊断的结论 */
export type BootVerdict =
  | 'healthy'
  | 'backupDamaged'
  | 'primaryDamaged'
  | 'bothDamaged'
  | 'unsupported'
  | 'unrecognized'

/** 一个问题：界面按 repair.issue.<code> 显示；value 是引导扇区里记的值，expected 是应有的值 */
export interface BootIssue {
  code: string
  value: number | null
  expected: number | null
}

/** 主备之间不一样的一个字段，`field` 是规范里的字段名，界面按 bootField.<field> 显示（与检查文件系统共用） */
export interface BootDifference {
  field: string
  primary: number
  backup: number
}

/** 一份引导扇区的核对结果；issues 为空表示这一份正常 */
export interface BootCopyReport {
  /** 在源上的字节偏移 */
  offset: number
  /** 让这一份不能用的问题；为空表示核对通过 */
  issues: BootIssue[]
  /** 不影响使用的问题，只作提醒 */
  notes: BootIssue[]
  volumeBytes: number | null
  clusterSize: number | null
  /** 引导扇区正常时按它打开了卷；打不开时 openError 是原因。有问题时不试，两项都为空 */
  opened: boolean
  openError: string | null
}

export interface PartitionBoot {
  number: number
  offset: number
  length: number
  typeId: string
  role: string | null
  name: string
  logical: boolean
  /** fat12、fat16、fat32、exfat、ntfs、bitlocker、ext、unknown */
  filesystem: string
  verdict: BootVerdict
  label: string | null
  primary: BootCopyReport
  /** FAT12/16 没有备份；认不出文件系统时也不找 */
  backup: BootCopyReport | null
  differences: BootDifference[]
  /** 分区表与引导扇区不符：typeMismatch、volumeLarger、volumeSmaller */
  tableIssues: BootIssue[]
}

export interface BootDiagnosis {
  size: number
  sectorSize: number
  scheme: 'mbr' | 'gpt' | 'none'
  notes: string[]
  partitions: PartitionBoot[]
}

/** 浏览分区时读出的分区表 */
export interface BrowseOpened {
  size: number
  sectorSize: number
  scheme: 'mbr' | 'gpt' | 'none'
  /** 读分区表时发现的问题，界面按 undelete.notes.* 显示 */
  notes: string[]
  partitions: PartitionInfo[]
}

export interface PartitionInfo {
  number: number
  offset: number
  length: number
  /** MBR 为 0x0C 这样的类型字节，GPT 为类型 GUID */
  typeId: string
  /** 常见类型的用途，界面按 partition.role.* 显示 */
  role: string | null
  /** GPT 分区名 */
  name: string
  bootable: boolean
  logical: boolean
  /** fat12、fat16、fat32、exfat、ntfs、bitlocker、ext、unknown、unreadable */
  filesystem: string
  label: string | null
  browsable: boolean
  /** 列出已删除的项之前要先 scanDeleted（NTFS） */
  needsScan: boolean
  /** 可以按需 scanDeleted（FAT、exFAT）：读一遍空闲簇，找出原来的目录已找不到的文件夹 */
  optionalScan: boolean
  /** 卷上的时间是 UTC（NTFS）；否则是写入时的本地时间（FAT、exFAT） */
  timesUtc: boolean
}

/** 扫描已删除的项的结果；cancelled 时其余各项为 0 */
export interface ScanResult {
  cancelled: boolean
  /** MFT 里的记录数、在用的记录数（FAT、exFAT 为 0） */
  records: number
  inUse: number
  /** 已删除、还留着内容的文件与目录记录数，以及它们的名字数（FAT、exFAT 是认出的文件夹里的项数） */
  deleted: number
  deletedNames: number
  /** 列在「找不到原目录的文件」里的项数：NTFS 是名字数，FAT、exFAT 是文件夹数 */
  orphans: number
  /** 读不出的记录数；FAT、exFAT 是读不出的簇数 */
  unreadable: number
  /** FAT、exFAT：读过的空闲空间字节数 */
  freeBytes: number
  /** NTFS：$LogFile 里认出的日志记录数（没有日志、读不出时为 0） */
  logRecords: number
  /** NTFS：从 $LogFile 找回的名字数（MFT 里那条记录已被重用或清掉的已删除项） */
  logNames: number
  elapsedMs: number
}

/** 扫描的种类：mft 是 NTFS 扫主文件表，free 是 FAT / exFAT 读空闲簇（进度的 stage 是 scanMft / scanFree） */
export type ScanKind = 'mft' | 'free'

/** 内容的存放方式暂不支持读出 */
export type UnsupportedReason = 'compressed' | 'encrypted' | 'systemCompressed' | 'reparse'

/**
 * live：未删除；intact：已删除、看起来完整；overwritten：已被覆盖；unknown：无法判断；
 * cleared：数据位置已不知道，读不出（ext3 / ext4 删除时清空了长度与数据位置；NTFS 从日志找回、日志里只有名字）
 */
export type EntryState = 'live' | 'intact' | 'overwritten' | 'unknown' | 'cleared'

/** 分区里的一项 */
export interface FsEntry {
  name: string
  shortName: string | null
  /** 名字有字符无法确定（FAT 删除后丢了首字母） */
  nameUncertain: boolean
  isDir: boolean
  size: number
  modified: string | null
  created: string | null
  deleted: boolean
  state: EntryState
  /** 引擎给的定位信息，原样传回 */
  file: string
  /** 数据在源上的起始字节偏移；没有数据、或还不知道时为空 */
  position: number | null
  /** 存放方式暂不支持读出（NTFS 压缩、加密等） */
  unsupported: UnsupportedReason | null
  /** NTFS 的 MFT 记录号 */
  record: number | null
  /** 不在卷上的虚拟目录：orphans 是「找不到原目录的文件」 */
  virtualDir: 'orphans' | null
  /** 带隐藏、系统属性（回收站、System Volume Information 等，资源管理器默认不显示） */
  hidden: boolean
  system: boolean
  /**
   * NTFS 从 $LogFile 找回：MFT 里那条记录已被重用或清掉，名字与数据位置来自日志。内容按删除前的运行表读，
   * 那些簇可能已被别的文件用了；state 为 cleared 时日志里只有名字，读不出
   */
  fromLog: boolean
}

/**
 * 分区里的一项能不能读出原内容（预览、打开）：文件、存放方式支持、数据位置还知道
 * （cleared 是删除时位置已被清空，或从日志找回时日志里只有名字）
 */
export function hasReadableContent(entry: FsEntry): boolean {
  return !entry.isDir && entry.unsupported === null && entry.state !== 'cleared'
}

export interface FileListPage {
  total: number
  entries: FsEntry[]
  truncated: boolean
  overwritten: boolean
}

/** 按位置打开的分区（丢失分区搜索找到的）编号从 100000 起，引擎据此不核对分区表、只核对范围在源之内 */
export interface PartitionRef {
  number: number
  offset: number
  length: number
}

/** 丢失分区搜索找到的一个分区 */
export interface FoundPartition {
  offset: number
  /** 卷的字节数，已截到源的末尾 */
  length: number
  /** fat12、fat16、fat32、exfat、ntfs */
  filesystem: string
  label: string | null
  /** 靠哪个引导扇区找到：primary（分区开头的）、backup（主引导扇区已坏，靠备份）、both */
  /** structure：主备引导扇区都没了，按主文件表的结构找到（只有 NTFS），只能浏览、复制 */
  foundBy: 'primary' | 'backup' | 'both' | 'structure'
  /** 浏览、复制时按哪一份引导扇区打开（只靠备份找到的为 backup）；原样传给 openBrowseRange 与 startCopy */
  boot: BootCopy
  /** 卷超出了源的末尾，长度已截短 */
  truncated: boolean
  /** 与现有分区表的关系：表里已有、与表里的分区重叠、丢失 */
  relation: 'inTable' | 'overlaps' | 'lost'
  /** inTable、overlaps 时是表里哪个分区 */
  tablePartition: number | null
  /** 与别的候选分区重叠：其中至少一个不是真的分区 */
  overlapsCandidate: boolean
}

/** 丢失分区怎么搜：quick 只看分区通常开始的位置、找到就跳过；deep 从头到尾读一遍 */
export type SearchMode = 'quick' | 'deep'

/** 丢失分区搜索的结果 */
export interface SearchSummary {
  size: number
  sectorSize: number
  mode: SearchMode
  /** 被停下时搜到的位置（字节）；停下之前找到的分区照样列出。搜完时为 null */
  stoppedAt: number | null
  /** 现有分区表：mbr、gpt、none；读不出时为 unreadable */
  scheme: 'mbr' | 'gpt' | 'none' | 'unreadable'
  /** 按起点排序 */
  candidates: FoundPartition[]
  candidatesTruncated: boolean
  unreadableBytes: number
  elapsedMs: number
}

/** 要复制的一项：定位信息，以及复制到输出目录下的相对路径（一级一项） */
export interface CopyItem {
  file: string
  path: string[]
}

/** 复制好（或复制失败）的一个文件 */
export interface CopiedFile {
  /** 输出目录下的相对路径，以 / 分隔 */
  path: string
  size: number
  error: string | null
  /** 复制出的内容全是 0（固态硬盘 TRIM 后的已删除文件常常如此） */
  allZero: boolean
  /** 保存只扫描的结果时，这个文件在源上的起始偏移：保存成的名字可能改过（另存为、重名加序号），按它对上是哪个文件 */
  offset?: number
}

export interface CopySummary {
  outputDir: string
  files: number
  bytes: number
  failed: number
  elapsedMs: number
}

export interface ByteRange {
  offset: number
  length: number
}

/** 任务读源的累计统计（从任务开始算），任务运行期间每秒一次，没有变化时不发 */
export interface ReadingStats {
  /** 完成的读取次数（含读错的） */
  reads: number
  bytes: number
  /** 读错的次数 */
  errors: number
  /** 单次用时超过 2 秒的读取次数：盘在内部反复重试，多半是坏扇区或快坏的扇区 */
  slowReads: number
  /** 花在读取上的总毫秒数 */
  busyMs: number
  /** 此刻已经连续读了多少毫秒还没读完；没在读时为 0 */
  stalledMs: number
}

export interface ReadScanReport {
  size: number
  bytesRead: number
  unreadable: ByteRange[]
  unreadableBytes: number
  unreadableRangesTruncated: boolean
  elapsedMs: number
  /** 扇区图：设备按 mapCellBytes 一格分开，每格的状态 */
  map: ReadMapCell[]
  mapCellBytes: number
  /** 细读过的 1 MiB 块数；细读到了上限，之后读不出的只按整块或当时那一段记 */
  fineReadBlocks: number
  fineReadTruncated: boolean
}

/**
 * 扇区图一格：都读出了、读出了但有一次超过 2 秒、有读不出的扇区、整格读不出；
 * 制作镜像另有整格没读、读了一部分（其余没读或待修边、细读）
 */
export type ReadMapCell = 'good' | 'slow' | 'partly' | 'unreadable' | 'untried' | 'pending'

/** 制作镜像的读取轮次：复制、修边、细读 */
export type ImagingRound = 'copy' | 'trim' | 'scrape'

/** 制作镜像的进展；各项字节数取自坏区图、只算本次的范围，续做时包括以前读的 */
export interface ImagingStatus {
  round: ImagingRound
  /** 本次范围的字节数；不限范围时是源的容量 */
  size: number
  readBytes: number
  badBytes: number
  untriedBytes: number
  trimBytes: number
  scrapeBytes: number
  /** 最近几秒的读速；刚开始时为 null */
  bytesPerSecond: number | null
  /** 只做复制这一轮 */
  copyOnly: boolean
  /** 到现在为止连续读错了几次（读成功一次就回到 0） */
  errorsInARow: number
  /** 连续读错到了要提示检查连接的次数（引擎判断） */
  checkConnection: boolean
  /** 整个源的扇区图（按坏区图，含本次范围以外的），分格同坏道检测 */
  map: ReadMapCell[]
  mapCellBytes: number
}

/** 制作镜像的结果；字节数只算本次的范围，device* 是整个源的 */
export interface ImageSummary {
  imagePath: string
  mapPath: string
  size: number
  readBytes: number
  badBytes: number
  /** 本次范围里还没读完的字节数：源设备读不到时，或者只做了复制这一轮时不为 0 */
  unfinishedBytes: number
  /** 本次范围里读不出的区段，相邻的已合并；最多 1000 段 */
  badRanges: ByteRange[]
  badRangesTruncated: boolean
  /** 本次只读了源的一部分 */
  partial: boolean
  copyOnly: boolean
  deviceSize: number
  deviceReadBytes: number
  deviceUnfinishedBytes: number
  /** 源设备读不到了（多半是断开），在这个偏移处停下；检查连接后可以续做 */
  sourceLostAt: number | null
  /** 很久没有读成功，在这个偏移处停下；可以续做 */
  stalledAt: number | null
  elapsedMs: number
  /** 整个源的扇区图，同 ImagingStatus.map */
  map: ReadMapCell[]
  mapCellBytes: number
}

/** 检查出的问题有多严重：会丢数据、结构错误、可以忽略 */
export type FsIssueSeverity = 'dataLoss' | 'structure' | 'minor'

/** 文件系统检查发现的一个问题；界面按 kind 给出说明与以后的修法，没用到的位置信息为 null */
export interface FsIssue {
  kind: string
  severity: FsIssueSeverity
  /** 问题出在哪个结构上（boot、fat、bitmap 等），按 fsCheck.areas.* 显示 */
  area: string | null
  /** 涉及的文件或目录，以 / 开头；$Bitmap、$UpCase 是 exFAT 的分配位图与大写表 */
  path: string | null
  /** 交叉链接的另一个文件 */
  otherPath: string | null
  cluster: number | null
  count: number | null
  /** 在源上的字节偏移（读不出的区段） */
  offset: number | null
  expected: number | null
  actual: number | null
  /** 引导扇区里出问题的那一项，如 bytesPerSector */
  field: string | null
}

/** 文件系统检查的结果 */
export interface FsCheckReport {
  /** fat12、fat16、fat32、exfat、ntfs、unknown */
  filesystem: string
  clusterSize: number
  clusterCount: number
  usedClusters: number
  files: number
  directories: number
  /** 按发现的顺序，最多 1000 项 */
  issues: FsIssue[]
  /** 没有列出的问题数 */
  omittedIssues: number
  /** 各严重程度的问题数，含没有列出的 */
  totals: Record<FsIssueSeverity, number>
  unreadableBytes: number
  /** 做了的与没能做的检查，按 fsCheck.checks.* 显示 */
  checked: string[]
  skipped: string[]
  elapsedMs: number
  /** FAT、exFAT 上能修的几类问题与各自多少；坏簇要先读空闲簇，不在里面 */
  repairs: FsFixCount[]
}

/** 宿主转发的任务事件；`completed` 的结果随任务种类而定（读取扫描为 ReadScanReport，深度恢复为 CarveSummary） */
export type JobEvent<TResult = unknown> =
  | {
      jobId: string
      kind: 'progress'
      /** stage：任务里的哪个阶段，只有一个阶段的任务不给；浏览与复制是 scanMft、scanLog 或 scanFree，文件系统检查见 FsCheckReport */
      data: { done: number; total: number; stage?: string }
    }
  | { jobId: string; kind: 'found'; data: { files: FoundFile[] } }
  | { jobId: string; kind: 'copied'; data: { files: CopiedFile[] } }
  | { jobId: string; kind: 'imaging'; data: ImagingStatus }
  /** 丢失分区搜索到目前为止找到的全部分区（有变化时发出，最后一份在结果里） */
  | { jobId: string; kind: 'partitions'; data: { candidates: FoundPartition[]; candidatesTruncated: boolean } }
  | { jobId: string; kind: 'reading'; data: ReadingStats }
  | { jobId: string; kind: 'log'; data: { level: string; message: string } }
  | { jobId: string; kind: 'completed'; data: { result: TResult } }
  | {
      jobId: string
      kind: 'failed'
      data: { message?: string; reason?: 'engineExited'; exitCode?: number | null }
    }
  | { jobId: string; kind: 'cancelled'; data: { forced?: boolean } }
  | { jobId: string; kind: 'unresponsive'; data: { silentSeconds: number } }
  | { jobId: string; kind: 'responsive'; data: Record<string, never> }

const JOB_EVENT = 'job-event'

export function getAppInfo(): Promise<AppInfo> {
  return invoke<AppInfo>('app_info')
}

/** 用默认浏览器打开本项目的发布页（网址由宿主决定） */
export function openReleasesPage(): Promise<void> {
  return invoke<void>('open_releases_page')
}

export function getEngineCapabilities(): Promise<EngineCapabilities> {
  return invoke<EngineCapabilities>('engine_capabilities')
}

/** 列出物理磁盘：只查询设备属性，不需要管理员权限 */
export async function listDisks(): Promise<DiskSummary[]> {
  const list = await invoke<{ disks: DiskSummary[] }>('disk_list')
  return list.disks
}

export async function listFileTypes(): Promise<FileType[]> {
  const list = await invoke<{ types: FileType[] }>('carve_file_types')
  return list.types
}

/** 任务 ID 由调用方生成：事件可能先于这个调用返回到达，调用前就要记下它 */
export function startReadScan(jobId: string, target: JobTarget): Promise<void> {
  return invoke<void>('job_start_read_scan', { jobId, target })
}

/**
 * 深度恢复：结果放在 outputDir 下新建的子目录里；outputDir 为 null 时只扫描、什么都不写，
 * 之后用 previewCarveRange 预览、startExtract 保存选中的文件。物理磁盘与读取扫描一样可能先等提权；
 * 输出目录在被扫描的盘上（或无法确认不在）时宿主返回 outputOnSource
 */
/**
 * 开始一次深度恢复，记进扫描记录（记录 ID 就是任务 ID）；sourceLabel 是来源描述，源没接上时也认得出。
 * keepIncomplete：部分恢复，不完整的文件也存下（标为不完整）；joinFragments：分成两段的照片也拼回来（较慢）
 */
export function startCarve(
  jobId: string,
  target: JobTarget,
  outputDir: string | null,
  extensions: string[],
  keepIncomplete: boolean,
  joinFragments: boolean,
  sourceLabel: string,
): Promise<void> {
  return invoke<void>('job_start_carve', {
    jobId,
    target,
    outputDir,
    extensions,
    keepIncomplete,
    joinFragments,
    sourceLabel,
  })
}

/** 扫描记录的状态：正在扫、停下了（可以接着扫）、扫完了、出错停下（可以接着扫） */
export type CarveStatus = 'running' | 'stopped' | 'completed' | 'failed'

/** 一次深度恢复扫描（含之后的续扫）的记录，程序关掉以后还在 */
export interface CarveRecord {
  id: string
  target: JobTarget
  sourceLabel: string
  extensions: string[]
  /** 部分恢复：不完整的文件也存下（接着扫时照样）；以前的记录为 false */
  keepIncomplete: boolean
  /** 两段重组：分成两段的照片也拼回来（接着扫时照样）；以前的记录没有这一项 */
  joinFragments?: boolean
  /** 边扫边保存时实际的目录（recovered-…）；只扫描时为 null */
  outputDir: string | null
  status: CarveStatus
  /** 接着扫的位置（字节） */
  resumeAt: number
  size: number
  files: number
  startedAtMs: number
  updatedAtMs: number
}

export interface CarveRecordOpened {
  record: CarveRecord
  files: FoundFile[]
}

/** 扫描记录，最近开始的在前 */
export function listCarveHistory(): Promise<CarveRecord[]> {
  return invoke<CarveRecord[]>('carve_history_list')
}

/** 打开一条扫描记录：之后按记录 ID 预览、另存、打开恢复出的文件 */
export function openCarveRecord(recordId: string): Promise<CarveRecordOpened> {
  return invoke<CarveRecordOpened>('carve_history_open', { recordId })
}

/** 删掉一条扫描记录（只删记录与找到的文件清单，不碰恢复出的文件）；正在扫的不能删（carveRecordRunning） */
export function removeCarveRecord(recordId: string): Promise<void> {
  return invoke<void>('carve_history_remove', { recordId })
}

/** 接着扫一次停下的扫描：同样的源与格式，从最后一个检查点接着扫，找到的文件记进同一条记录 */
export function resumeCarve(jobId: string, recordId: string): Promise<void> {
  return invoke<void>('job_resume_carve', { jobId, recordId })
}

/** 只扫描时找到的一个文件的位置、长度与格式；name 是另存为时起的文件名，不给时用恢复时起的 */
export interface ExtractItem {
  offset: number
  length: number
  extension: string
  name?: string
  /** 扫描时是不完整的文件：原样带回，保存时按部分恢复核对 */
  incomplete?: IncompleteReason
  /** 扫描时由两段拼成的文件：原样带回，保存时把这几段接起来核对 */
  fragments?: FileFragment[]
}

/**
 * 预览只扫描、还没保存的文件：宿主回到扫描任务的源上读这一段（由两段拼成的按 `fragments` 接起来读），
 * 由引擎生成预览
 */
export function previewCarveRange(
  scanJobId: string,
  offset: number,
  length: number,
  fragments?: FileFragment[],
): Promise<FilePreview> {
  return invoke<FilePreview>('carve_preview_range', { scanJobId, offset, length, fragments })
}

/**
 * 保存只扫描时选中的文件（任务）：回到源上重新读出并校验，与扫描时对不上的不保存。
 * 文件直接存进 outputDir，重名时加序号、不覆盖；outputDir 为 null 时是为打开、复制而保存，
 * 宿主存到程序文件夹下 data 里的 open-cache 并在后台清掉以前的副本。
 * 保存好的文件按这个任务的 jobId 加保存成的文件名打开、在文件夹中显示
 */
export function startExtract(
  jobId: string,
  scanJobId: string,
  items: ExtractItem[],
  outputDir: string | null,
): Promise<void> {
  return invoke<void>('job_start_extract', { jobId, scanJobId, items, outputDir })
}

export function cancelJob(jobId: string): Promise<void> {
  return invoke<void>('job_cancel', { jobId })
}

/** 暂停或继续任务；目前只有深度恢复的扫描与文件系统检查可以暂停 */
export function setJobPaused(jobId: string, paused: boolean): Promise<void> {
  return invoke<void>('job_set_paused', { jobId, paused })
}

/** 深度恢复找到的文件按任务 ID 加文件名指定：宿主只认恢复时起的文件名，不接受其他路径 */
export function previewRecoveredFile(jobId: string, name: string): Promise<FilePreview> {
  return invoke<FilePreview>('recovered_preview', { jobId, name })
}

/** 用系统默认程序打开 */
export function openRecoveredFile(jobId: string, name: string): Promise<void> {
  return invoke<void>('recovered_open', { jobId, name })
}

/** 系统的「打开方式」对话框：用户挑一个程序打开。可执行与脚本类文件不打开（executableBlocked） */
export function openRecoveredFileWith(jobId: string, name: string): Promise<void> {
  return invoke<void>('recovered_open_with', { jobId, name })
}

/** 把文件放到剪贴板，之后可以在资源管理器里粘贴 */
export function copyRecoveredFile(jobId: string, name: string): Promise<void> {
  return invoke<void>('recovered_copy', { jobId, name })
}

/** 在系统的文件管理器里显示 */
export function revealRecoveredFile(jobId: string, name: string): Promise<void> {
  return invoke<void>('recovered_reveal', { jobId, name })
}

/**
 * 浏览分区（找回删除的文件）：读出分区表后引擎一直运行，用 cancelJob 关闭。
 * 物理磁盘与扫描一样可能先等提权。boot 为 backup 时各分区按备份引导扇区打开（修复打不开的分区）
 */
export function openBrowse(
  jobId: string,
  target: JobTarget,
  boot: BootCopy = 'primary',
): Promise<BrowseOpened> {
  return invoke<BrowseOpened>('browse_open', { jobId, target, boot })
}

/** 列出浏览中的一个目录的一页；dir 为 null 时是根目录。名字很长时引擎给的项数可能少于 limit */
export function listFiles(
  jobId: string,
  partition: number,
  dir: string | null,
  offset: number,
  limit: number,
): Promise<FileListPage> {
  return invoke<FileListPage>('browse_list', { jobId, partition, dir, offset, limit })
}

/** 预览浏览中的一个文件，规则与深度恢复的预览相同 */
export function previewFsFile(jobId: string, partition: number, file: string): Promise<FilePreview> {
  return invoke<FilePreview>('browse_preview', { jobId, partition, file })
}

/**
 * 找出浏览中的一个分区里已删除的项（NTFS 扫一遍 MFT，再读 $LogFile；FAT、exFAT 读一遍空闲簇，找原来的目录已找不到的文件夹），
 * 扫完才返回；进度经浏览任务的 progress 事件送达（stage 为 scanMft、scanLog 或 scanFree，换阶段时进度从 0 重新算；
 * 总量还不知道时 total 为 0）。
 * 扫过的分区直接返回上次的结果
 */
export function scanDeleted(jobId: string, partition: number): Promise<ScanResult> {
  return invoke<ScanResult>('browse_scan', { jobId, partition })
}

/** 停下正在进行的扫描；那次 scanDeleted 返回 cancelled */
export function cancelScan(jobId: string, partition: number): Promise<void> {
  return invoke<void>('browse_cancel_scan', { jobId, partition })
}

/** 搜索丢失的分区（只读，可以暂停）；物理磁盘与扫描一样可能先等提权。结果经 completed 事件送达 */
/** 被取消时也以 completed 结束，结果里是停下之前找到的分区 */
export function startPartitionSearch(jobId: string, target: JobTarget, mode: SearchMode): Promise<void> {
  return invoke<void>('job_start_partition_search', { jobId, target, mode })
}

/**
 * 在浏览会话里按位置打开一个分区（搜索找到的、不在分区表里）。答复的编号之后像表里的分区一样
 * 用于 listFiles、previewFsFile、scanDeleted；同一位置再打开时编号相同
 */
export function openBrowseRange(
  jobId: string,
  offset: number,
  length: number,
  boot: BootCopy,
): Promise<PartitionInfo> {
  return invoke<PartitionInfo>('browse_open_range', { jobId, offset, length, boot })
}

/**
 * 把浏览中选中的项复制到 outputDir 下新建的子目录里（另存为）。源为物理磁盘时，
 * 输出目录在源盘上（或无法确认不在）会返回 outputOnSource。
 * outputDir 为 null 时是为打开而复制：宿主存到程序文件夹下 data 里的 open-cache 并在后台清掉以前的副本
 */
export function startCopy(
  jobId: string,
  target: JobTarget,
  partition: PartitionRef,
  items: CopyItem[],
  outputDir: string | null,
  boot: BootCopy = 'primary',
): Promise<void> {
  return invoke<void>('job_start_copy', { jobId, target, partition, items, outputDir, boot })
}

/**
 * 诊断各分区的引导扇区：读出主引导扇区与备份逐项核对，只读，读完即返回。
 * 物理磁盘第一次读时可能先等提权
 */
export function diagnoseBoot(target: JobTarget): Promise<BootDiagnosis> {
  return invoke<BootDiagnosis>('boot_diagnose', { target })
}

/**
 * 修复引导扇区的方向：用备份覆盖坏了的主引导扇区，或用主引导扇区重写坏了的备份；
 * rebuild：主备都坏了时按分区里的结构推算出引导扇区，主引导扇区与备份都写（卷序列号换成新的）
 */
export type BootFix = 'primaryFromBackup' | 'backupFromPrimary' | 'rebuild'

/** 重建出的引导扇区：推算出的各项（规范字段名，按 bootField.* 显示）、卷标、按它打开后根目录的前几项与总数 */
export interface RebuiltBoot {
  fields: { field: string; value: number }[]
  label: string | null
  root: string[]
  rootCount: number
}

/** 执行写入计划的步骤，失败时停在其中一步 */
export type WriteStep = 'validate' | 'identity' | 'verify' | 'write' | 'flush' | 'readBack'

/** 要写的一段在源上的位置（字节） */
export interface WriteRange {
  offset: number
  length: number
}

/** 修复引导扇区的细节：from、to 是源上的字节偏移，length 是复制的字节数（重建时是第一段的长度） */
export interface BootSectorDetail {
  partition: number
  partitionOffset: number
  partitionLength: number
  filesystem: string
  fix: BootFix
  from: number
  to: number
  length: number
  rebuilt: RebuiltBoot | null
}

/** 怎么改分区表：把搜索找到的丢失分区写回去，或把类型与文件系统不符的分区改成对的类型 */
export type PartitionTableFix =
  | { kind: 'addLost'; offset: number; length: number }
  | { kind: 'setType'; partition: number }
  /** 主 GPT 或备份 GPT 坏了：用完好的那一份修好它（方向由引擎定） */
  | { kind: 'repairGpt' }

/** 改写分区表的细节：改完之后这个分区的编号、位置与类型（MBR 为 0x07 这样的类型字节，GPT 为类型 GUID） */
export interface PartitionTableDetail {
  fix: PartitionTableFix
  scheme: 'mbr' | 'gpt'
  filesystem: string
  partition: number
  offset: number
  length: number
  typeId: string
  /** 改完之后的用途，界面按 partition.role.* 显示 */
  role: string | null
  /** 修 GPT 的方向；此时上面的分区编号、位置为 0 */
  gptRepair: 'primaryFromBackup' | 'backupFromPrimary' | null
}

/** NTFS `$MFT` 与 `$MFTMirr` 互相修复的方向 */
export type MftFix = 'mirrorFromMft' | 'mftFromMirror'

/** `$MFT` 与 `$MFTMirr` 互修的细节：改写哪几条记录（0 到 3），从哪一份的起点复制到哪一份 */
export interface MftDetail {
  fix: MftFix
  partition: number
  partitionOffset: number
  partitionLength: number
  records: number[]
  from: number
  to: number
}

/**
 * 文件系统修复（FAT、exFAT）的一类问题：FSInfo、分配位图里漏标的簇、过长的链、丢失簇（先另存）、未正常卸下的标记；
 * 两份 FAT 不一致（以第一份 / 第二份为准，单独做）、断了的链、交叉链接、目录项、读不出的空闲簇（先读一遍空闲簇）；
 * exFAT 的大写表（写回标准表）、分配位图本身坏了（按目录树重建，单独做）
 */
export type FsFix =
  | 'fsInfo'
  | 'bitmap'
  | 'tooLong'
  | 'lost'
  | 'dirty'
  | 'fatFromFirst'
  | 'fatFromSecond'
  | 'chains'
  | 'crossLinks'
  | 'entries'
  | 'badClusters'
  | 'upcase'
  | 'rebuildBitmap'

/** 一段连续的簇 */
export interface ClusterRun {
  first: number
  count: number
}

/** 以某一份 FAT 为准统一之后还剩多少问题 */
export interface FatRating {
  fix: 'fatFromFirst' | 'fatFromSecond'
  totals: Record<FsIssueSeverity, number>
  /** 要改的扇区数 */
  sectors: number
}

/** 读空闲簇的结果：读不出的簇最多列 10000 段，unreadableClusters 含没列出的 */
export interface FreeScanSummary {
  clusters: number
  bytes: number
  unreadable: ClusterRun[]
  unreadableClusters: number
  elapsedMs: number
}

/** 一类修复改了多少：items 是文件数、段数或处数，clusters 是簇数 */
export interface FsFixCount {
  fix: FsFix
  items: number
  clusters: number
}

export interface FsFixDetail {
  partition: number
  partitionOffset: number
  partitionLength: number
  filesystem: string
  counts: FsFixCount[]
}
/** 写入计划的摘要；回滚时 bootSector / partitionTable / mft / fsFix 是原来那次修复的细节，rollbackOf 是它的计划 ID */
export interface PlanSummary {
  kind: 'bootSector' | 'partitionTable' | 'mft' | 'fsFix' | 'rollback'
  target: JobTarget
  writes: WriteRange[]
  bootSector: BootSectorDetail | null
  partitionTable: PartitionTableDetail | null
  mft: MftDetail | null
  fsFix: FsFixDetail | null
  /** 写到哪块盘（旧记录没有） */
  device: PlanDevice | null
  rollbackOf: string | null
}

/** 等确认的写入计划：备份已经存好，确认后按计划 ID 执行（要写的内容存在宿主里） */
export interface PlanView extends PlanSummary {
  planId: string
  backupPath: string
}

export interface WriteOutcome {
  done: boolean
  /** 失败时停在哪一步 */
  step: WriteStep | null
  /** 失败前已经写下去的段数；为 0 时源没被改动。成功时是全部段数 */
  written: number
  detail: string | null
}

/** 写盘记录里的一条：执行过的计划（成功或失败） */
/**
 * 写到哪块盘：认盘标识、容量、扇区大小与计划里的设备相同；model 是厂商与型号、serial 是序列号（镜像文件、查不到时为空）。
 * 写盘记录按它认源，不按磁盘编号
 */
export interface PlanDevice {
  identity: string
  size: number
  sectorSize: number
  model: string
  serial: string
}

/** 一个源的写盘记录；indistinct：这块盘查不到序列号，同型号、同容量的另一块盘的记录也会列在里面 */
export interface WriteHistory {
  records: WriteRecord[]
  indistinct: boolean
}

export interface WriteRecord extends PlanSummary {
  planId: string
  backupPath: string
  executedAtMs: number
  outcome: WriteOutcome
}

/**
 * 生成修复引导扇区的写入计划：引擎重新诊断、预演写入，宿主存好备份后返回，不写盘。
 * 目前只支持镜像文件（物理盘、卷返回 writeTargetUnsupported）；诊断结论已经变了时返回 planNotApplicable
 */
export function planBootSector(target: JobTarget, partition: number, fix: BootFix): Promise<PlanView> {
  return invoke<PlanView>('write_plan_boot_sector', { target, partition, fix })
}

/**
 * 生成改写分区表的写入计划（只读，不写盘）。那个位置认不出文件系统、分区表没有空位或与别的分区重叠时
 * 返回 planUnavailable；类型本来就对时返回 planNotApplicable
 */
export function planPartitionTable(target: JobTarget, fix: PartitionTableFix): Promise<PlanView> {
  return invoke<PlanView>('write_plan_partition_table', { target, fix })
}

/**
 * 生成 `$MFT` 与 `$MFTMirr` 互相修复的写入计划（只读，不写盘）；分区与检查文件系统时相同。
 * 没有可以这样修的记录时返回 planNotApplicable
 */
export function planMft(target: JobTarget, partition: PartitionRef, fix: MftFix): Promise<PlanView> {
  return invoke<PlanView>('write_plan_mft', { target, partition, fix })
}

/** 生成文件系统修复（勾选的几类问题）的计划；引擎要把分区检查两遍，大分区要等一会儿 */
/** 生成文件系统修复的计划；badClusters 是读空闲簇时读不出的簇（只在修坏簇时用，引擎再读一次核对） */
export function planFsFix(
  target: JobTarget,
  partition: PartitionRef,
  fixes: FsFix[],
  badClusters: ClusterRun[] = [],
): Promise<PlanView> {
  return invoke<PlanView>('write_plan_fs_fix', { target, partition, fixes, badClusters })
}

/** 两份 FAT 不一致时，以第一份、以第二份为准各自修完还剩多少问题（只读，要把分区检查三遍） */
export function rateFatCopies(target: JobTarget, partition: PartitionRef): Promise<FatRating[]> {
  return invoke<FatRating[]>('fs_rate_fat_copies', { target, partition })
}

/** 把分区里的空闲簇读一遍，找出读不出的（任务，只读）；结果是 FreeScanSummary */
export function startScanFree(jobId: string, target: JobTarget, partition: PartitionRef): Promise<void> {
  return invoke<void>('job_start_scan_free', { jobId, target, partition })
}

/**
 * 把分区里的丢失簇另存到 outputDir 下新建的 lost-<任务 ID 前 8 位> 目录（任务，只读源），
 * 每段连续的丢失簇存成一个 lost-<起始簇>.bin；结果是 CopySummary。源为物理磁盘时输出目录不能在这块盘上
 */
export function startSaveLost(
  jobId: string,
  target: JobTarget,
  partition: PartitionRef,
  outputDir: string,
): Promise<void> {
  return invoke<void>('job_start_save_lost', { jobId, target, partition, outputDir })
}

/** 执行已确认的计划；镜像正被别的程序打开时返回 imageInUse，计划留着可以再试 */
export function executeWritePlan(planId: string): Promise<WriteRecord> {
  return invoke<WriteRecord>('write_plan_execute', { planId })
}

/** 放弃还没执行的计划（删掉它的备份） */
export function discardWritePlan(planId: string): Promise<void> {
  return invoke<void>('write_plan_discard', { planId })
}

/**
 * 为写盘记录里的一次写入生成回滚计划，同样要确认后执行。target 是现在查看的源，必须就是当时写的那块盘
 * （不是时返回 rollbackOtherDevice）
 */
export function planRollback(planId: string, target: JobTarget): Promise<PlanView> {
  return invoke<PlanView>('write_plan_rollback', { planId, target })
}

/** 这个源（现在这块盘、这个镜像文件）上的写盘记录，最新的在前；宿主按认盘标识认，不按磁盘编号 */
export function listWriteHistory(target: JobTarget): Promise<WriteHistory> {
  return invoke<WriteHistory>('write_history_list', { target })
}

/** 用系统默认程序打开复制出的文件；可执行与脚本类文件宿主不打开（executableBlocked） */
export function openCopiedFile(jobId: string, path: string): Promise<void> {
  return invoke<void>('copied_open', { jobId, path })
}

export function revealCopiedFile(jobId: string, path: string): Promise<void> {
  return invoke<void>('copied_reveal', { jobId, path })
}

/**
 * 打开另存出来的文件；宿主不打开可执行与脚本类文件（executableBlocked）时，改为在文件夹中显示。
 * 返回 revealed 时界面要说明为什么没打开。深度恢复与分区浏览共用这一个做法
 */
export async function openOrReveal(
  open: () => Promise<void>,
  reveal: () => Promise<void>,
): Promise<'opened' | 'revealed'> {
  try {
    await open()
    return 'opened'
  } catch (error) {
    if (toHostError(error).code !== 'executableBlocked') {
      throw error
    }
  }
  await reveal()
  return 'revealed'
}

/** 这一次怎样做镜像：只读哪些范围（不给时读整个源）、先清掉哪些范围的进度、是否只做复制这一轮 */
export interface ImagePlan {
  ranges?: ByteRange[]
  reset?: ByteRange[]
  copyOnly?: boolean
  /** 读取参数；不给的项用引擎的默认值 */
  tuning?: Partial<ImagingTuning>
}

/** 制作镜像的读取参数（「高级选项」），引擎核对范围（不对时 invalidParams） */
export interface ImagingTuning {
  /** 第 1 轮每次读多少字节：64 KiB 到 8 MiB */
  blockBytes: number
  /** 第 1 轮读错后起步跳过多少字节（连续出错时加倍） */
  skipBytes: number
  /** 第 4 轮把坏扇区再读几遍，0 不做（最多 10） */
  retries: number
  /** 一次读超过这么多毫秒，当作走到了坏区边上：数据照收，跳过一段先读别处；0 不看 */
  slowReadMs: number
  /** 连续这么多毫秒没有一次读成功就停下（可以续做）；0 不停 */
  stallLimitMs: number
}

/**
 * 制作能识别坏区的镜像（resume 时按同一路径的坏区图续做）。新建时镜像或坏区图已存在返回 imageExists，
 * 续做时坏区图不是这个源做的返回 imageMismatch；物理磁盘的镜像在同一块盘上时返回 outputOnSource。
 * 开始后宿主把镜像记进做过的镜像列表，sourceLabel 是给人看的来源描述
 */
export function startImage(
  jobId: string,
  target: JobTarget,
  imagePath: string,
  resume: boolean,
  plan: ImagePlan,
  sourceLabel: string,
): Promise<void> {
  return invoke<void>('job_start_image', { jobId, target, imagePath, resume, plan, sourceLabel })
}

/** 一段里各状态的字节数（取自坏区图；还没有镜像时全部未读） */
export interface RegionProgress {
  readBytes: number
  badBytes: number
  untriedBytes: number
  trimBytes: number
  scrapeBytes: number
}

/** 抽查：从镜像里已读出的部分取几块与源比较；mismatched 不为 0 说明做镜像之后源上这一段被写过 */
export interface RegionCheck {
  checked: number
  mismatched: number
  unreadable: number
}

/** 源上的一段：分区（partition 是编号）或分区以外的空隙（partition 为 null） */
export interface ImageRegion {
  partition: number | null
  offset: number
  length: number
  progress: RegionProgress
  check: RegionCheck | null
}

export interface PartitionChange {
  kind: 'added' | 'removed' | 'moved'
  number: number
  before: ByteRange | null
  after: ByteRange | null
}

/** 这个位置已有的镜像；state 不是 ok 时不能续做 */
export interface ExistingImage {
  state: 'ok' | 'imageMissing' | 'mapMissing' | 'mapUnreadable' | 'otherSource' | 'lengthMismatch'
  detail: string | null
  sourceIdentity: string | null
  /** 镜像是稀疏文件：可以只读一部分；旧版本建的或建在不支持稀疏文件的卷上的不是 */
  sparse: boolean
  /** 坏区图里记了上次开始读时的分区表（旧版本做的没有） */
  partitionsRecorded: boolean
  partitionChanges: PartitionChange[]
  progress: RegionProgress
}

/** 做镜像之前看到的情况 */
export interface ImageInspection {
  size: number
  sectorSize: number
  scheme: 'mbr' | 'gpt' | 'none' | null
  partitions: PartitionInfo[]
  regions: ImageRegion[]
  /** 只读一部分时总会一起读的分区表区域 */
  tableAreas: ByteRange[]
  output: { fileSystem: string; sparse: boolean; freeBytes: number } | null
  outputError: string | null
  existing: ExistingImage | null
}

/**
 * 做镜像之前看一眼源与镜像（只读）：源上的分区与空隙各做到哪了、输出位置能不能只读一部分、
 * 已有镜像能不能续做、做镜像之后源变没变。物理磁盘第一次需要时弹出 UAC；镜像在源盘上时返回 outputOnSource
 */
export function inspectImage(target: JobTarget, imagePath: string): Promise<ImageInspection> {
  return invoke<ImageInspection>('image_inspect', { target, imagePath })
}

/** 做过的一个镜像的记录 */
export interface ImageRecord {
  imagePath: string
  target: JobTarget
  sourceIdentity: string
  sourceLabel: string
  size: number
  usedAtMs: number
}

/** 以前做过的镜像现在的样子 */
export interface ImageDescription {
  imagePath: string
  /** missing：镜像与坏区图都不在了（被删除或移走） */
  state: 'ok' | 'missing' | 'imageMissing' | 'mapMissing' | 'mapUnreadable'
  detail: string | null
  sourceIdentity: string | null
  size: number | null
  progress: RegionProgress | null
  savedAtMs: number | null
}

export interface ImageHistoryItem {
  record: ImageRecord
  status: ImageDescription
}

/** 做过的镜像，最近用的在前，各带现在的状态 */
export function listImageHistory(): Promise<ImageHistoryItem[]> {
  return invoke<ImageHistoryItem[]>('image_history_list')
}

/** 从做过的镜像列表移除（不删除文件） */
export function removeImageHistory(imagePath: string): Promise<void> {
  return invoke<void>('image_history_remove', { imagePath })
}

/** 在文件夹中显示做过的镜像；只认列表里记着的路径（否则 imageRecordNotFound） */
export function revealImage(imagePath: string): Promise<void> {
  return invoke<void>('image_reveal', { imagePath })
}

/**
 * 镜像被移走后重新指定位置：新位置旁边要有同名的 .map 坏区图（否则 imageRelocateInvalid），
 * 而且来源与容量与原来一致（否则 imageRelocateMismatch）
 */
export function relocateImageHistory(imagePath: string, newPath: string): Promise<ImageHistoryItem> {
  return invoke<ImageHistoryItem>('image_history_relocate', { imagePath, newPath })
}

/**
 * 检查分区的文件系统（任务，只读）：结果是 FsCheckReport，进度按阶段（boot、fat、directories、clusters、mft）送达。
 * 分区与源上的分区表对不上时返回 fsUnavailable；物理磁盘与扫描一样可能先等提权
 */
export function startFsCheck(jobId: string, target: JobTarget, partition: PartitionRef): Promise<void> {
  return invoke<void>('job_start_fs_check', { jobId, target, partition })
}

/**
 * 把问题清单（界面按当前语言生成的文本）存到另存为对话框选的 .txt 文件。
 * 检查的是物理磁盘时，存到这块盘上会返回 outputOnSource
 */
export function saveFsCheckReport(target: JobTarget, path: string, text: string): Promise<void> {
  return invoke<void>('fs_check_save_report', { target, path, text })
}

/** 调用方按自己发起的任务种类给出 TResult：宿主不在事件里标注结果的类型 */
export function onJobEvent<TResult>(
  handler: (event: JobEvent<TResult>) => void,
): Promise<UnlistenFn> {
  return listen<JobEvent<TResult>>(JOB_EVENT, (event) => handler(event.payload))
}

/** 系统的选择文件夹对话框；用户取消时返回 null */
export function pickDirectory(title: string): Promise<string | null> {
  return open({ title, directory: true, multiple: false })
}

/** 系统的选择文件对话框；用户取消时返回 null */
export function pickFile(title: string): Promise<string | null> {
  return open({ title, directory: false, multiple: false })
}

/** 选镜像文件时对话框里的几组类型：常见的镜像扩展名（含 E01）、只看 E01、所有文件（raw 镜像可以是任何扩展名） */
export interface ImageFileFilters {
  images: string
  ewf: string
  all: string
}

/** 系统的选择文件对话框，按镜像文件过滤；用户取消时返回 null */
export function pickImageFile(title: string, names: ImageFileFilters): Promise<string | null> {
  return open({
    title,
    directory: false,
    multiple: false,
    filters: [
      { name: names.images, extensions: ['img', 'dd', 'raw', 'bin', 'iso', 'vhd', '001', 'e01'] },
      { name: names.ewf, extensions: ['e01'] },
      { name: names.all, extensions: ['*'] },
    ],
  })
}

/** 系统的「另存为」对话框：defaultPath 是建议的目录加文件名，只列出 extension 类型；用户取消时返回 null */
export function pickSaveFile(
  title: string,
  defaultPath: string,
  extension: { name: string; extension: string },
): Promise<string | null> {
  return save({
    title,
    defaultPath,
    filters: [{ name: extension.name, extensions: [extension.extension] }],
  })
}

/** 命令失败时，宿主抛出的是 HostError；其他情况（例如命令不存在）是字符串 */
export function toHostError(error: unknown): HostError {
  if (typeof error === 'object' && error !== null) {
    const { code, detail } = error as Record<string, unknown>
    if (typeof code === 'string' && typeof detail === 'string') {
      return { code, detail }
    }
  }
  return { code: 'unknown', detail: String(error) }
}
