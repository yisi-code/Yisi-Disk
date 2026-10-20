import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import type { UnlistenFn } from '@tauri-apps/api/event'
import {
  cancelJob,
  onJobEvent,
  startReadScan,
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

/** 一次读取扫描的状态：发起、取消，以及按任务事件更新进度和结果 */
export function useReadScan() {
  const status = ref<ReadScanStatus>('idle')
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

  async function start(path: string) {
    if (isBusy.value) {
      return
    }
    const id = crypto.randomUUID()
    jobId = id
    startedAt = performance.now()
    status.value = 'starting'
    done.value = 0
    total.value = 0
    bytesPerSecond.value = null
    report.value = null
    failure.value = null
    isUnresponsive.value = false
    try {
      await startReadScan(id, path)
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
    if (jobId === null || (status.value !== 'starting' && status.value !== 'running')) {
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
      case 'log':
        break
    }
  }

  return {
    status,
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
