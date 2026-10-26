// 与宿主通信的唯一入口：组件不直接调用 Tauri 命令
import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { open } from '@tauri-apps/plugin-dialog'

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
}

/** 任务的源：镜像文件，或按编号指定的物理磁盘 */
export type JobTarget = { kind: 'image'; path: string } | { kind: 'disk'; diskNumber: number }

/** 深度恢复支持的一种格式；扩展名同时是格式的标识 */
export interface FileType {
  extension: string
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
  files: number
  bytes: number
  /** 按扩展名统计 */
  byType: Record<string, TypeTotal>
  unreadableBytes: number
  elapsedMs: number
}

/** 深度恢复找到的一个文件 */
export interface FoundFile {
  name: string
  extension: string
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
  /** 卷上的时间是 UTC（NTFS）；否则是写入时的本地时间（FAT、exFAT） */
  timesUtc: boolean
}

/** 扫描已删除的项的结果；cancelled 时其余各项为 0 */
export interface ScanResult {
  cancelled: boolean
  /** MFT 里的记录数、在用的记录数 */
  records: number
  inUse: number
  /** 已删除、还留着内容的文件与目录记录数，以及它们的名字数 */
  deleted: number
  deletedNames: number
  /** 原来的目录已找不到的名字数 */
  orphans: number
  /** 读不出的记录数 */
  unreadable: number
  elapsedMs: number
}

/** 内容的存放方式暂不支持读出 */
export type UnsupportedReason = 'compressed' | 'encrypted' | 'systemCompressed' | 'reparse'

/** live：未删除；intact：已删除、看起来完整；overwritten：已被覆盖；unknown：无法判断 */
export type EntryState = 'live' | 'intact' | 'overwritten' | 'unknown'

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
}

export interface FileListPage {
  total: number
  entries: FsEntry[]
  truncated: boolean
  overwritten: boolean
}

export interface PartitionRef {
  number: number
  offset: number
  length: number
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

export interface ReadScanReport {
  size: number
  bytesRead: number
  unreadable: ByteRange[]
  unreadableBytes: number
  unreadableRangesTruncated: boolean
  elapsedMs: number
}

/** 宿主转发的任务事件；`completed` 的结果随任务种类而定（读取扫描为 ReadScanReport，深度恢复为 CarveSummary） */
export type JobEvent<TResult = unknown> =
  | {
      jobId: string
      kind: 'progress'
      /** stage：任务里的哪个阶段，只有一个阶段的任务不给；目前只有 scanMft（扫描 MFT） */
      data: { done: number; total: number; stage?: string }
    }
  | { jobId: string; kind: 'found'; data: { files: FoundFile[] } }
  | { jobId: string; kind: 'copied'; data: { files: CopiedFile[] } }
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
export function startImageScan(jobId: string, path: string): Promise<void> {
  return invoke<void>('job_start_image_scan', { jobId, path })
}

/** 第一次扫描物理磁盘时会弹出 UAC，这个调用一直等到用户作答、磁盘打开为止 */
export function startDiskScan(jobId: string, diskNumber: number): Promise<void> {
  return invoke<void>('job_start_disk_scan', { jobId, diskNumber })
}

/**
 * 深度恢复：结果放在 outputDir 下新建的子目录里；outputDir 为 null 时只扫描、什么都不写，
 * 之后用 previewCarveRange 预览、startExtract 保存选中的文件。物理磁盘与读取扫描一样可能先等提权；
 * 输出目录在被扫描的盘上（或无法确认不在）时宿主返回 outputOnSource
 */
export function startCarve(
  jobId: string,
  target: JobTarget,
  outputDir: string | null,
  extensions: string[],
): Promise<void> {
  return invoke<void>('job_start_carve', { jobId, target, outputDir, extensions })
}

/** 只扫描时找到的一个文件的位置、长度与格式 */
export interface ExtractItem {
  offset: number
  length: number
  extension: string
}

/** 预览只扫描、还没保存的文件：宿主回到扫描任务的源上读这一段，由引擎生成预览 */
export function previewCarveRange(
  scanJobId: string,
  offset: number,
  length: number,
): Promise<FilePreview> {
  return invoke<FilePreview>('carve_preview_range', { scanJobId, offset, length })
}

/**
 * 保存只扫描时选中的文件（任务）：回到源上重新读出并校验，与扫描时对不上的不保存。
 * 保存好的文件按这个任务的 jobId 加文件名打开、在文件夹中显示
 */
export function startExtract(
  jobId: string,
  scanJobId: string,
  items: ExtractItem[],
  outputDir: string,
): Promise<void> {
  return invoke<void>('job_start_extract', { jobId, scanJobId, items, outputDir })
}

export function cancelJob(jobId: string): Promise<void> {
  return invoke<void>('job_cancel', { jobId })
}

/** 深度恢复找到的文件按任务 ID 加文件名指定：宿主只认恢复时起的文件名，不接受其他路径 */
export function previewRecoveredFile(jobId: string, name: string): Promise<FilePreview> {
  return invoke<FilePreview>('recovered_preview', { jobId, name })
}

/** 用系统默认程序打开 */
export function openRecoveredFile(jobId: string, name: string): Promise<void> {
  return invoke<void>('recovered_open', { jobId, name })
}

/** 在系统的文件管理器里显示 */
export function revealRecoveredFile(jobId: string, name: string): Promise<void> {
  return invoke<void>('recovered_reveal', { jobId, name })
}

/**
 * 浏览分区（找回删除的文件）：读出分区表后引擎一直运行，用 cancelJob 关闭。
 * 物理磁盘与扫描一样可能先等提权
 */
export function openBrowse(jobId: string, target: JobTarget): Promise<BrowseOpened> {
  return invoke<BrowseOpened>('browse_open', { jobId, target })
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
 * 找出浏览中的一个分区里已删除的项（NTFS 扫一遍 MFT），扫完才返回；
 * 进度经浏览任务的 progress 事件送达（stage 为 scanMft）。扫过的分区直接返回上次的结果
 */
export function scanDeleted(jobId: string, partition: number): Promise<ScanResult> {
  return invoke<ScanResult>('browse_scan', { jobId, partition })
}

/** 停下正在进行的扫描；那次 scanDeleted 返回 cancelled */
export function cancelScan(jobId: string, partition: number): Promise<void> {
  return invoke<void>('browse_cancel_scan', { jobId, partition })
}

/**
 * 把浏览中选中的项复制到 outputDir 下新建的子目录里。源为物理磁盘时，
 * 输出目录在源盘上（或无法确认不在）会返回 outputOnSource
 */
export function startCopy(
  jobId: string,
  target: JobTarget,
  partition: PartitionRef,
  items: CopyItem[],
  outputDir: string,
): Promise<void> {
  return invoke<void>('job_start_copy', { jobId, target, partition, items, outputDir })
}

/** 用系统默认程序打开复制出的文件；可执行与脚本类文件宿主不打开（executableBlocked） */
export function openCopiedFile(jobId: string, path: string): Promise<void> {
  return invoke<void>('copied_open', { jobId, path })
}

export function revealCopiedFile(jobId: string, path: string): Promise<void> {
  return invoke<void>('copied_reveal', { jobId, path })
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
