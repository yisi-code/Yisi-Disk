import { ref, shallowRef } from 'vue'
import { useJob } from './useJob'
import {
  startCarve,
  type CarveSummary,
  type FoundFile,
  type JobTarget,
} from '../services/host'

/** 一次深度恢复：在通用任务状态之上，记下全部找到的文件，并在结束时记下恢复报告 */
export function useCarve() {
  const targetKind = ref<JobTarget['kind']>('image')
  const foundCount = ref(0)
  /**
   * 按找到的先后（即起始位置）排列，可能有十万级。每批换一个新数组：子组件按引用判断 prop 是否变了，
   * 原地 push 不会让它更新；批次至多 250 毫秒一次，复制引用的开销可以忽略
   */
  const files = shallowRef<FoundFile[]>([])
  const summary = ref<CarveSummary | null>(null)

  const job = useJob<CarveSummary>({
    onFound: recordFound,
    onCompleted(result) {
      summary.value = result
    },
  })

  /** outputDir 为 null 时只扫描、什么都不写 */
  async function start(target: JobTarget, outputDir: string | null, extensions: string[]) {
    if (job.isBusy.value) {
      return
    }
    targetKind.value = target.kind
    foundCount.value = 0
    files.value = []
    summary.value = null
    await job.start((jobId) => startCarve(jobId, target, outputDir, extensions))
  }

  function recordFound(batch: FoundFile[]) {
    foundCount.value += batch.length
    files.value = files.value.concat(batch)
  }

  return {
    jobId: job.jobId,
    status: job.status,
    targetKind,
    done: job.done,
    total: job.total,
    percent: job.percent,
    bytesPerSecond: job.bytesPerSecond,
    foundCount,
    files,
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
