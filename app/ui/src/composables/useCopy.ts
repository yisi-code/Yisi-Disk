import { computed, ref, shallowRef } from 'vue'
import { useJob } from './useJob'
import {
  startCopy,
  type CopiedFile,
  type CopyItem,
  type CopySummary,
  type JobTarget,
  type PartitionRef,
} from '../services/host'

/** 一次复制：在通用任务状态之上，记下陆续复制好（或失败）的文件和最后的汇总 */
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

  async function start(
    target: JobTarget,
    partition: PartitionRef,
    items: CopyItem[],
    outputDir: string,
  ) {
    if (job.isBusy.value) {
      return
    }
    copied.value = []
    summary.value = null
    await job.start((jobId) => startCopy(jobId, target, partition, items, outputDir))
  }

  return {
    jobId: job.jobId,
    status: job.status,
    done: job.done,
    total: job.total,
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
