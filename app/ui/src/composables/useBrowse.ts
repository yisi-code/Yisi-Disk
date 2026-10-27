import { computed, onBeforeUnmount, ref, shallowRef } from 'vue'
import { useJob } from './useJob'
import {
  cancelJob,
  cancelScan,
  openBrowse,
  openBrowseRange,
  scanDeleted,
  toHostError,
  type BootCopy,
  type BrowseOpened,
  type HostError,
  type JobTarget,
  type PartitionInfo,
  type ScanResult,
} from '../services/host'

/**
 * 浏览分区的会话：引擎读出分区表后一直运行，直到关闭（取消任务）、引擎出错或宿主断开。
 * 无响应、意外退出沿用通用任务状态；离开页面时自动关闭。
 * NTFS 分区要先扫描 MFT 才列得出已删除的项：扫描期间的进度是这个任务的进度。
 */
export function useBrowse() {
  const opened = shallowRef<BrowseOpened | null>(null)
  /** 打开会话时的源：复制任务要用同一个源 */
  const target = shallowRef<JobTarget | null>(null)
  /** 各分区扫描已删除项的结果；关闭会话时清空。每次换一个新 Map，按引用判断是否变了 */
  const scanResults = shallowRef(new Map<number, ScanResult>())
  /** 正在扫描的分区 */
  const scanningPartition = ref<number | null>(null)
  const isCancellingScan = ref(false)
  const scanFailure = ref<HostError | null>(null)
  // 浏览任务不会「完成」，只会被关闭或失败
  // 浏览会话开着时多半在等用户点选；扫描 MFT 时才算在干活
  const job = useJob<null>({ onCompleted() {} }, { isIdle: () => !isScanning.value })

  const isOpen = computed(() => job.status.value === 'running' && opened.value !== null)
  /** 正在打开或正在关闭，这时不能再打开新的会话 */
  const isTransitioning = computed(
    () => job.status.value === 'starting' || job.status.value === 'cancelling',
  )
  const isScanning = computed(() => scanningPartition.value !== null)

  onBeforeUnmount(() => {
    if (job.status.value === 'running' && job.jobId.value !== null) {
      void cancelJob(job.jobId.value).catch(() => undefined)
    }
  })

  /** bootCopy 为 backup 时各分区按备份引导扇区打开 */
  async function open(source: JobTarget, bootCopy: BootCopy = 'primary') {
    if (job.isBusy.value) {
      return
    }
    opened.value = null
    target.value = source
    resetScans()
    await job.start(async (jobId) => {
      opened.value = await openBrowse(jobId, source, bootCopy)
    })
  }

  async function close() {
    opened.value = null
    resetScans()
    await job.cancel()
  }

  /** 按位置打开一个不在分区表里的分区（丢失分区搜索找到的），之后用答复里的编号浏览 */
  async function openRange(
    offset: number,
    length: number,
    boot: BootCopy,
  ): Promise<PartitionInfo | null> {
    const jobId = job.jobId.value
    if (jobId === null || !isOpen.value) {
      return null
    }
    return openBrowseRange(jobId, offset, length, boot)
  }

  /** 扫描分区里已删除的项；扫过的分区引擎直接给出上次的结果。被停下或失败时返回 null */
  async function scan(partition: number): Promise<ScanResult | null> {
    const jobId = job.jobId.value
    if (jobId === null || !isOpen.value || isScanning.value) {
      return null
    }
    scanFailure.value = null
    scanningPartition.value = partition
    try {
      const result = await scanDeleted(jobId, partition)
      if (result.cancelled) {
        return null
      }
      scanResults.value = new Map(scanResults.value).set(partition, result)
      return result
    } catch (error) {
      // 关闭会话时引擎退出，扫描请求随之失败：这不算扫描出错
      if (job.jobId.value === jobId && isOpen.value) {
        scanFailure.value = toHostError(error)
      }
      return null
    } finally {
      scanningPartition.value = null
      isCancellingScan.value = false
    }
  }

  async function stopScan() {
    const jobId = job.jobId.value
    const partition = scanningPartition.value
    if (jobId === null || partition === null || isCancellingScan.value) {
      return
    }
    isCancellingScan.value = true
    try {
      await cancelScan(jobId, partition)
    } catch {
      // 扫描恰好在停下之前结束：结果照常返回
      isCancellingScan.value = false
    }
  }

  function resetScans() {
    scanResults.value = new Map()
    scanFailure.value = null
  }

  return {
    jobId: job.jobId,
    status: job.status,
    opened,
    target,
    failure: job.failure,
    isUnresponsive: job.isUnresponsive,
    readHealth: job.readHealth,
    isOpen,
    isTransitioning,
    scanResults,
    scanningPartition,
    isScanning,
    isCancellingScan,
    scanFailure,
    scanDone: job.done,
    scanTotal: job.total,
    scanPercent: job.percent,
    open,
    close,
    openRange,
    scan,
    stopScan,
  }
}
