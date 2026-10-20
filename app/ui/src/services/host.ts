// 与宿主通信的唯一入口：组件不直接调用 Tauri 命令
import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'

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

export type JobEvent =
  | { jobId: string; kind: 'progress'; data: { done: number; total: number } }
  | { jobId: string; kind: 'log'; data: { level: string; message: string } }
  | { jobId: string; kind: 'completed'; data: { result: ReadScanReport } }
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

/** 任务 ID 由调用方生成：事件可能先于这个调用返回到达，调用前就要记下它 */
export function startImageScan(jobId: string, path: string): Promise<void> {
  return invoke<void>('job_start_image_scan', { jobId, path })
}

/** 第一次扫描物理磁盘时会弹出 UAC，这个调用一直等到用户作答、磁盘打开为止 */
export function startDiskScan(jobId: string, diskNumber: number): Promise<void> {
  return invoke<void>('job_start_disk_scan', { jobId, diskNumber })
}

export function cancelJob(jobId: string): Promise<void> {
  return invoke<void>('job_cancel', { jobId })
}

export function onJobEvent(handler: (event: JobEvent) => void): Promise<UnlistenFn> {
  return listen<JobEvent>(JOB_EVENT, (event) => handler(event.payload))
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
