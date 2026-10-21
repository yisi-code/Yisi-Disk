import { ref } from 'vue'
import { useJob } from './useJob'
import {
  startCarve,
  type CarveSummary,
  type FoundFile,
  type JobTarget,
  type TypeTotal,
} from '../services/host'

/** 「最近找到的文件」最多列这么多个，最新的在前 */
const RECENT_FILE_LIMIT = 8

/** 一次深度恢复：在通用任务状态之上，边找边按类型统计，并在结束时记下恢复报告 */
export function useCarve() {
  const targetKind = ref<JobTarget['kind']>('image')
  const foundCount = ref(0)
  const foundByType = ref<Record<string, TypeTotal>>({})
  const recentFiles = ref<FoundFile[]>([])
  const summary = ref<CarveSummary | null>(null)

  const job = useJob<CarveSummary>({
    onFound: recordFound,
    onCompleted(result) {
      summary.value = result
    },
  })

  async function start(target: JobTarget, outputDir: string, extensions: string[]) {
    if (job.isBusy.value) {
      return
    }
    targetKind.value = target.kind
    foundCount.value = 0
    foundByType.value = {}
    recentFiles.value = []
    summary.value = null
    await job.start((jobId) => startCarve(jobId, target, outputDir, extensions))
  }

  /** 一批新找到的文件：先算好统计再一次性替换，界面不会看到算了一半的数字 */
  function recordFound(files: FoundFile[]) {
    const totals = { ...foundByType.value }
    for (const file of files) {
      const current = totals[file.extension] ?? { count: 0, bytes: 0 }
      totals[file.extension] = { count: current.count + 1, bytes: current.bytes + file.length }
    }
    foundByType.value = totals
    foundCount.value += files.length
    recentFiles.value = [...files].reverse().concat(recentFiles.value).slice(0, RECENT_FILE_LIMIT)
  }

  return {
    status: job.status,
    targetKind,
    done: job.done,
    total: job.total,
    percent: job.percent,
    bytesPerSecond: job.bytesPerSecond,
    foundCount,
    foundByType,
    recentFiles,
    summary,
    failure: job.failure,
    isUnresponsive: job.isUnresponsive,
    isBusy: job.isBusy,
    start,
    cancel: job.cancel,
  }
}
