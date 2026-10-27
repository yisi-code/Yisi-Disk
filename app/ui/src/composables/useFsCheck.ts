import { shallowRef } from 'vue'
import { useJob } from './useJob'
import {
  startFsCheck,
  type FsCheckReport,
  type JobTarget,
  type PartitionInfo,
} from '../services/host'

/** 一次文件系统检查：在通用任务状态之上记下检查的源、分区和结果 */
export function useFsCheck() {
  const report = shallowRef<FsCheckReport | null>(null)
  /** 检查时的源与分区：检查前会关闭浏览会话，分区表随之清空，这里留一份 */
  const target = shallowRef<JobTarget | null>(null)
  const partition = shallowRef<PartitionInfo | null>(null)

  const job = useJob<FsCheckReport>({
    onCompleted(result) {
      report.value = result
    },
  })

  async function start(source: JobTarget, checked: PartitionInfo) {
    if (job.isBusy.value) {
      return
    }
    report.value = null
    target.value = source
    partition.value = checked
    const partitionRef = { number: checked.number, offset: checked.offset, length: checked.length }
    await job.start((jobId) => startFsCheck(jobId, source, partitionRef))
  }

  return {
    status: job.status,
    stage: job.stage,
    done: job.done,
    total: job.total,
    percent: job.percent,
    failure: job.failure,
    isUnresponsive: job.isUnresponsive,
    readHealth: job.readHealth,
    isPaused: job.isPaused,
    isTogglingPause: job.isTogglingPause,
    isBusy: job.isBusy,
    report,
    target,
    partition,
    start,
    cancel: job.cancel,
    pause: job.pause,
    resume: job.resume,
  }
}
