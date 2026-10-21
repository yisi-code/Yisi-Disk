import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import type { UnlistenFn } from '@tauri-apps/api/event'
import {
  cancelJob,
  onJobEvent,
  startDiskScan,
  startImageScan,
  toHostError,
  type HostError,
  type JobEvent,
  type ReadScanReport,
} from '../services/host'

export type ReadScanStatus =
  | 'idle'
  | 'starting'
  | 'running'
  | 'cancelling'
  | 'completed'
  | 'failed'
  | 'cancelled'

/** 扫描对象：镜像文件，或按编号指定的物理磁盘 */
export type ScanTarget = { kind: 'image'; path: string } | { kind: 'disk'; diskNumber: number }

/** 一次读取扫描的状态：发起、取消，以及按任务事件更新进度和结果 */
export function useReadScan() {
  const status = ref<ReadScanStatus>('idle')
  const targetKind = ref<ScanTarget['kind']>('image')
  const done = ref(0)
  const total = ref(0)
  const bytesPerSecond = ref<number | null>(null)
  const report = ref<ReadScanReport | null>(null)
  const failure = ref<HostError | null>(null)
  const isUnresponsive = ref(false)

  let jobId: string | null = null
  let startedAt = 0
  let unlisten: UnlistenFn | null = null
  let isDisposed = false

  const isBusy = computed(
    () => status.value === 'starting' || status.value === 'running' || status.value === 'cancelling',
  )
  const percent = computed(() =>
    total.value > 0 ? Math.floor((done.value / total.value) * 100) : 0,
  )

  onMounted(async () => {
    const stop = await onJobEvent(handleJobEvent)
    // 组件在监听建立前已卸载时，立即取消监听
    if (isDisposed) {
      stop()
    } else {
      unlisten = stop
    }
  })

  onBeforeUnmount(() => {
    isDisposed = true
    unlisten?.()
  })

  async function start(target: ScanTarget) {
    if (isBusy.value) {
      return
    }
    const id = crypto.randomUUID()
    jobId = id
    startedAt = performance.now()
    targetKind.value = target.kind
    status.value = 'starting'
    done.value = 0
    total.value = 0
    bytesPerSecond.value = null
    report.value = null
    failure.value = null
    isUnresponsive.value = false
    try {
      if (target.kind === 'image') {
        await startImageScan(id, target.path)
      } else {
        await startDiskScan(id, target.diskNumber)
      }
      if (jobId === id && status.value === 'starting') {
        status.value = 'running'
      }
    } catch (error) {
      if (jobId === id) {
        status.value = 'failed'
        failure.value = toHostError(error)
      }
    }
  }

  async function cancel() {
    // 启动阶段（可能正在等 UAC）引擎还没接下任务，只能在运行中取消
    if (jobId === null || status.value !== 'running') {
      return
    }
    status.value = 'cancelling'
    try {
      await cancelJob(jobId)
    } catch {
      // 任务恰好在取消前结束：终结事件会更新状态，这里只撤回「正在取消」
      if (status.value === 'cancelling') {
        status.value = 'running'
      }
    }
  }

  function handleJobEvent(event: JobEvent) {
    if (event.jobId !== jobId) {
      return
    }
    switch (event.kind) {
      case 'progress': {
        // 物理磁盘可能先等了一段 UAC，速度从第一次进度算起
        if (event.data.done === 0) {
          startedAt = performance.now()
        }
        const seconds = (performance.now() - startedAt) / 1000
        done.value = event.data.done
        total.value = event.data.total
        bytesPerSecond.value = seconds > 0 ? event.data.done / seconds : null
        if (status.value === 'starting') {
          status.value = 'running'
        }
        break
      }
      case 'completed':
        report.value = event.data.result
        done.value = event.data.result.size
        total.value = event.data.result.size
        isUnresponsive.value = false
        status.value = 'completed'
        break
      case 'failed':
        failure.value =
          event.data.reason === 'engineExited'
            ? { code: 'engineExited', detail: `exit code ${event.data.exitCode ?? '?'}` }
            : { code: 'jobFailed', detail: event.data.message ?? '' }
        isUnresponsive.value = false
        status.value = 'failed'
        break
      case 'cancelled':
        isUnresponsive.value = false
        status.value = 'cancelled'
        break
      case 'unresponsive':
        isUnresponsive.value = true
        break
      case 'responsive':
        isUnresponsive.value = false
        break
      case 'found':
      case 'log':
        break
    }
  }

  return {
    status,
    targetKind,
    done,
    total,
    percent,
    bytesPerSecond,
    report,
    failure,
    isUnresponsive,
    isBusy,
    start,
    cancel,
  }
}
