import { computed, onBeforeUnmount, shallowRef } from 'vue'
import { useJob } from './useJob'
import { cancelJob, openBrowse, type BrowseOpened, type JobTarget } from '../services/host'

/**
 * 浏览分区的会话：引擎读出分区表后一直运行，直到关闭（取消任务）、引擎出错或宿主断开。
 * 无响应、意外退出沿用通用任务状态；离开页面时自动关闭。
 */
export function useBrowse() {
  const opened = shallowRef<BrowseOpened | null>(null)
  /** 打开会话时的源：复制任务要用同一个源 */
  const target = shallowRef<JobTarget | null>(null)
  // 浏览任务不会「完成」，只会被关闭或失败
  const job = useJob<null>({ onCompleted() {} })

  const isOpen = computed(() => job.status.value === 'running' && opened.value !== null)
  /** 正在打开或正在关闭，这时不能再打开新的会话 */
  const isTransitioning = computed(
    () => job.status.value === 'starting' || job.status.value === 'cancelling',
  )

  onBeforeUnmount(() => {
    if (job.status.value === 'running' && job.jobId.value !== null) {
      void cancelJob(job.jobId.value).catch(() => undefined)
    }
  })

  async function open(source: JobTarget) {
    if (job.isBusy.value) {
      return
    }
    opened.value = null
    target.value = source
    await job.start(async (jobId) => {
      opened.value = await openBrowse(jobId, source)
    })
  }

  async function close() {
    opened.value = null
    await job.cancel()
  }

  return {
    jobId: job.jobId,
    status: job.status,
    opened,
    target,
    failure: job.failure,
    isUnresponsive: job.isUnresponsive,
    isOpen,
    isTransitioning,
    open,
    close,
  }
}
