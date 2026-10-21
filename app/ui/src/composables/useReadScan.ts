import { ref } from 'vue'
import { useJob } from './useJob'
import {
  startDiskScan,
  startImageScan,
  type JobTarget,
  type ReadScanReport,
} from '../services/host'

/** 一次读取扫描：在通用任务状态之上记下扫描对象的种类和扫描报告 */
export function useReadScan() {
  const targetKind = ref<JobTarget['kind']>('image')
  const report = ref<ReadScanReport | null>(null)

  const job = useJob<ReadScanReport>({
    onCompleted(result) {
      report.value = result
      job.done.value = result.size
      job.total.value = result.size
    },
  })

  async function start(target: JobTarget) {
    if (job.isBusy.value) {
      return
    }
    targetKind.value = target.kind
    report.value = null
    await job.start((jobId) =>
      target.kind === 'image'
        ? startImageScan(jobId, target.path)
        : startDiskScan(jobId, target.diskNumber),
    )
  }

  return {
    status: job.status,
    targetKind,
    done: job.done,
    total: job.total,
    percent: job.percent,
    bytesPerSecond: job.bytesPerSecond,
    report,
    failure: job.failure,
    isUnresponsive: job.isUnresponsive,
    isBusy: job.isBusy,
    start,
    cancel: job.cancel,
  }
}
