import { computed, ref, shallowRef } from 'vue'
import { useJob } from './useJob'
import type { CopiedFile, CopySummary } from '../services/host'

/**
 * 一次复制或保存：在通用任务状态之上，记下陆续复制好（或失败）的文件和最后的汇总。
 * 「找回删除的文件」的复制与深度恢复「只扫描」后的保存都用它，各自给出启动方式
 */
export function useCopy() {
  /** 每批换一个新数组，子组件按引用判断是否变了 */
  const copied = shallowRef<CopiedFile[]>([])
  const summary = ref<CopySummary | null>(null)

  const job = useJob<CopySummary>({
    onCopied(files) {
      copied.value = copied.value.concat(files)
    },
    onCompleted(result) {
      summary.value = result
    },
  })

  const failures = computed(() => copied.value.filter((file) => file.error !== null))
  const succeeded = computed(() => copied.value.filter((file) => file.error === null))

  /** `launch` 拿到新生成的任务 ID，调用宿主启动任务 */
  async function start(launch: (jobId: string) => Promise<void>) {
    if (job.isBusy.value) {
      return
    }
    copied.value = []
    summary.value = null
    await job.start(launch)
  }

  return {
    jobId: job.jobId,
    status: job.status,
    done: job.done,
    total: job.total,
    stage: job.stage,
    percent: job.percent,
    bytesPerSecond: job.bytesPerSecond,
    copied,
    failures,
    succeeded,
    summary,
    failure: job.failure,
    isUnresponsive: job.isUnresponsive,
    isBusy: job.isBusy,
    start,
    cancel: job.cancel,
  }
}
