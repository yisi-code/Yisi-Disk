import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import type { UnlistenFn } from '@tauri-apps/api/event'
import {
  cancelJob,
  onJobEvent,
  setJobPaused,
  toHostError,
  type CopiedFile,
  type FoundFile,
  type HostError,
  type ImagingStatus,
  type JobEvent,
} from '../services/host'

export type JobStatus =
  | 'idle'
  | 'starting'
  | 'running'
  | 'cancelling'
  | 'completed'
  | 'failed'
  | 'cancelled'

/**
 * 各类任务自己的结果处理：任务完成时交出结果，深度恢复陆续交出找到的文件，复制陆续交出复制好的文件，
 * 制作镜像陆续交出读取进展
 */
export interface JobHandlers<TResult> {
  onCompleted: (result: TResult) => void
  onFound?: (files: FoundFile[]) => void
  onCopied?: (files: CopiedFile[]) => void
  onImaging?: (status: ImagingStatus) => void
}

/**
 * 一个任务的通用状态：发起、取消、暂停与继续（只有支持的任务）、进度与速度、无响应与恢复、终结。
 * 任务 ID 在调用宿主之前生成并记下，事件先于启动命令返回到达时也不会丢；不属于当前任务的事件一律忽略。
 */
export function useJob<TResult>(handlers: JobHandlers<TResult>) {
  const status = ref<JobStatus>('idle')
  const done = ref(0)
  const total = ref(0)
  /** 进度所属的阶段（例如复制前先扫描 MFT 的 scanMft）；只有一个阶段的任务为 null */
  const stage = ref<string | null>(null)
  const bytesPerSecond = ref<number | null>(null)
  const failure = ref<HostError | null>(null)
  const isUnresponsive = ref(false)
  /** 运行中被暂停：状态仍是 running，引擎在检查点等着 */
  const isPaused = ref(false)
  /** 暂停或继续的请求还没答复 */
  const isTogglingPause = ref(false)
  /** 最近一次发起的任务；任务结束后保留，深度恢复的结果还要按它预览、打开 */
  const jobId = ref<string | null>(null)

  let startedAt = 0
  let pausedAt = 0
  let unlisten: UnlistenFn | null = null
  let isDisposed = false

  const isBusy = computed(
    () => status.value === 'starting' || status.value === 'running' || status.value === 'cancelling',
  )
  const percent = computed(() =>
    total.value > 0 ? Math.floor((done.value / total.value) * 100) : 0,
  )

  onMounted(async () => {
    const stop = await onJobEvent<TResult>(handleJobEvent)
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

  /** `launch` 拿到新生成的任务 ID，调用宿主启动任务。正在运行时什么也不做 */
  async function start(launch: (jobId: string) => Promise<void>) {
    if (isBusy.value) {
      return
    }
    const id = crypto.randomUUID()
    jobId.value = id
    startedAt = performance.now()
    status.value = 'starting'
    done.value = 0
    total.value = 0
    stage.value = null
    bytesPerSecond.value = null
    failure.value = null
    isUnresponsive.value = false
    isPaused.value = false
    try {
      await launch(id)
      if (jobId.value === id && status.value === 'starting') {
        status.value = 'running'
      }
    } catch (error) {
      if (jobId.value === id) {
        status.value = 'failed'
        failure.value = toHostError(error)
      }
    }
  }

  async function cancel() {
    // 启动阶段（可能正在等 UAC）引擎还没接下任务，只能在运行中取消
    if (jobId.value === null || status.value !== 'running') {
      return
    }
    status.value = 'cancelling'
    try {
      await cancelJob(jobId.value)
    } catch {
      // 任务恰好在取消前结束：终结事件会更新状态，这里只撤回「正在取消」
      if (status.value === 'cancelling') {
        status.value = 'running'
      }
    }
  }

  /** 暂停或继续；请求失败（例如任务恰好结束）时保持原状 */
  async function setPaused(paused: boolean) {
    const id = jobId.value
    if (id === null || status.value !== 'running' || isPaused.value === paused || isTogglingPause.value) {
      return
    }
    isTogglingPause.value = true
    try {
      await setJobPaused(id, paused)
      if (jobId.value !== id || status.value !== 'running') {
        return
      }
      isPaused.value = paused
      if (paused) {
        pausedAt = performance.now()
      } else {
        // 读速只算运行的时间：起点顺延暂停了的那一段
        startedAt += performance.now() - pausedAt
      }
    } catch {
      // 任务恰好结束：终结事件会更新状态
    } finally {
      isTogglingPause.value = false
    }
  }

  function handleJobEvent(event: JobEvent<TResult>) {
    if (event.jobId !== jobId.value) {
      return
    }
    switch (event.kind) {
      case 'progress': {
        const eventStage = event.data.stage ?? null
        // 物理磁盘可能先等了一段 UAC，换了阶段也要重算；速度从这一阶段的第一次进度算起
        if (event.data.done === 0 || eventStage !== stage.value) {
          startedAt = performance.now()
        }
        stage.value = eventStage
        const seconds = (performance.now() - startedAt) / 1000
        done.value = event.data.done
        total.value = event.data.total
        bytesPerSecond.value = seconds > 0 ? event.data.done / seconds : null
        if (status.value === 'starting') {
          status.value = 'running'
        }
        break
      }
      case 'found':
        handlers.onFound?.(event.data.files)
        break
      case 'copied':
        handlers.onCopied?.(event.data.files)
        break
      case 'imaging':
        handlers.onImaging?.(event.data)
        break
      case 'completed':
        handlers.onCompleted(event.data.result)
        isUnresponsive.value = false
        isPaused.value = false
        status.value = 'completed'
        break
      case 'failed':
        failure.value =
          event.data.reason === 'engineExited'
            ? { code: 'engineExited', detail: `exit code ${event.data.exitCode ?? '?'}` }
            : { code: 'jobFailed', detail: event.data.message ?? '' }
        isUnresponsive.value = false
        isPaused.value = false
        status.value = 'failed'
        break
      case 'cancelled':
        isUnresponsive.value = false
        isPaused.value = false
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
    jobId,
    status,
    done,
    total,
    stage,
    percent,
    bytesPerSecond,
    failure,
    isUnresponsive,
    isPaused,
    isTogglingPause,
    isBusy,
    start,
    cancel,
    pause: () => setPaused(true),
    resume: () => setPaused(false),
  }
}
