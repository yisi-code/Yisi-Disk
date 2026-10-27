import { ref } from 'vue'
import { useJob } from './useJob'
import { startPartitionSearch, type JobTarget, type SearchSummary } from '../services/host'

/** 一次丢失分区搜索：在通用任务状态之上记下搜索的源和结果；可以暂停 */
export function usePartitionSearch() {
  /** 发起搜索时的源：浏览找到的分区要用同一个源 */
  const target = ref<JobTarget | null>(null)
  const summary = ref<SearchSummary | null>(null)

  const job = useJob<SearchSummary>({
    onCompleted(result) {
      summary.value = result
    },
  })

  async function start(source: JobTarget) {
    if (job.isBusy.value) {
      return
    }
    target.value = source
    summary.value = null
    await job.start((jobId) => startPartitionSearch(jobId, source))
  }

  return {
    status: job.status,
    target,
    done: job.done,
    total: job.total,
    percent: job.percent,
    bytesPerSecond: job.bytesPerSecond,
    summary,
    failure: job.failure,
    isUnresponsive: job.isUnresponsive,
    isPaused: job.isPaused,
    isTogglingPause: job.isTogglingPause,
    isBusy: job.isBusy,
    start,
    cancel: job.cancel,
    pause: job.pause,
    resume: job.resume,
  }
}
