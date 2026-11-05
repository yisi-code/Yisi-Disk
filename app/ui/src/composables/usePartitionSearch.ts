import { ref } from 'vue'
import { useJob } from './useJob'
import {
  startPartitionSearch,
  type FoundPartition,
  type JobTarget,
  type SearchMode,
  type SearchSummary,
} from '../services/host'

/**
 * 一次丢失分区搜索：在通用任务状态之上记下搜索的源、边搜边找到的分区和结果；可以暂停。
 * 取消就是停止：引擎照样给出结果，里面是停下之前找到的分区
 */
export function usePartitionSearch() {
  /** 发起搜索时的源：浏览找到的分区要用同一个源 */
  const target = ref<JobTarget | null>(null)
  /** 搜索进行中到目前为止找到的分区 */
  const found = ref<FoundPartition[]>([])
  const summary = ref<SearchSummary | null>(null)

  const job = useJob<SearchSummary>({
    onPartitions(candidates) {
      found.value = candidates
    },
    onCompleted(result) {
      found.value = result.candidates
      summary.value = result
    },
  })

  async function start(source: JobTarget, mode: SearchMode) {
    if (job.isBusy.value) {
      return
    }
    target.value = source
    found.value = []
    summary.value = null
    await job.start((jobId) => startPartitionSearch(jobId, source, mode))
  }

  return {
    status: job.status,
    target,
    done: job.done,
    total: job.total,
    percent: job.percent,
    bytesPerSecond: job.bytesPerSecond,
    progressParams: job.progressParams,
    found,
    summary,
    failure: job.failure,
    isUnresponsive: job.isUnresponsive,
    readHealth: job.readHealth,
    isPaused: job.isPaused,
    isTogglingPause: job.isTogglingPause,
    isBusy: job.isBusy,
    start,
    cancel: job.cancel,
    pause: job.pause,
    resume: job.resume,
  }
}
