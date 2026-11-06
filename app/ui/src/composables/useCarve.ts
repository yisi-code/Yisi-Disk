import { ref, shallowRef } from 'vue'
import { useJob } from './useJob'
import {
  openCarveRecord,
  resumeCarve,
  startCarve,
  type CarveRecord,
  type CarveSummary,
  type FoundFile,
  type JobTarget,
} from '../services/host'

/**
 * 一次深度恢复：在通用任务状态之上，记下全部找到的文件，并在结束时记下恢复报告。
 * 每次扫描都有扫描记录（宿主存着）：可以打开以前的记录接着挑选保存，停下的可以接着扫
 */
export function useCarve() {
  const targetKind = ref<JobTarget['kind']>('image')
  const foundCount = ref(0)
  /**
   * 按找到的先后（即起始位置）排列，可能有十万级。每批换一个新数组：子组件按引用判断 prop 是否变了，
   * 原地 push 不会让它更新；批次至多 250 毫秒一次，复制引用的开销可以忽略
   */
  const files = shallowRef<FoundFile[]>([])
  const summary = ref<CarveSummary | null>(null)
  /** 扫描记录的 ID：预览、另存、打开恢复出的文件都按它（续扫的任务另有任务 ID，记录不变） */
  const recordId = ref<string | null>(null)
  /** 从扫描记录打开的那条（没在扫时按它显示状态）；新开始的扫描为 null */
  const record = ref<CarveRecord | null>(null)
  /** 已有文件的起点：续扫可能从上次已经报过的文件前面接着扫，同一个起点只算一次 */
  let offsets = new Set<number>()

  const job = useJob<CarveSummary>({
    onFound: recordFound,
    onCompleted(result) {
      summary.value = result
    },
  })

  function reset(kind: JobTarget['kind'], initial: FoundFile[]) {
    targetKind.value = kind
    files.value = initial
    offsets = new Set(initial.map((file) => file.offset))
    foundCount.value = initial.length
    summary.value = null
  }

  /** outputDir 为 null 时只扫描、什么都不写；keepIncomplete 时不完整的文件也存下；sourceLabel 记进扫描记录 */
  async function start(
    target: JobTarget,
    outputDir: string | null,
    extensions: string[],
    keepIncomplete: boolean,
    sourceLabel: string,
  ) {
    if (job.isBusy.value) {
      return
    }
    reset(target.kind, [])
    record.value = null
    await job.start((jobId) => {
      recordId.value = jobId
      return startCarve(jobId, target, outputDir, extensions, keepIncomplete, sourceLabel)
    })
  }

  /** 打开一条扫描记录：找到的文件与它的状态，之后可以挑选保存；出错时抛出 */
  async function open(id: string) {
    if (job.isBusy.value) {
      return
    }
    const opened = await openCarveRecord(id)
    reset(opened.record.target.kind, opened.files)
    recordId.value = id
    record.value = opened.record
  }

  /** 接着扫当前这条记录（停下或出错的），从它最后的检查点开始，找到的文件接在后面 */
  async function resumeScan() {
    const id = recordId.value
    if (job.isBusy.value || id === null) {
      return
    }
    summary.value = null
    await job.start((jobId) => resumeCarve(jobId, id))
  }

  function recordFound(batch: FoundFile[]) {
    const fresh = batch.filter((file) => !offsets.has(file.offset))
    if (fresh.length === 0) {
      return
    }
    for (const file of fresh) {
      offsets.add(file.offset)
    }
    foundCount.value += fresh.length
    files.value = files.value.concat(fresh)
  }

  return {
    jobId: job.jobId,
    status: job.status,
    targetKind,
    done: job.done,
    total: job.total,
    percent: job.percent,
    bytesPerSecond: job.bytesPerSecond,
    progressParams: job.progressParams,
    foundCount,
    files,
    summary,
    recordId,
    record,
    failure: job.failure,
    isUnresponsive: job.isUnresponsive,
    readHealth: job.readHealth,
    isPaused: job.isPaused,
    isTogglingPause: job.isTogglingPause,
    isBusy: job.isBusy,
    start,
    open,
    resumeScan,
    cancel: job.cancel,
    pause: job.pause,
    resume: job.resume,
  }
}
