import { ref } from 'vue'
import { useJob } from './useJob'
import {
  startImage,
  type ImageSummary,
  type ImagingStatus,
  type JobTarget,
} from '../services/host'

/** 一次制作镜像的源与镜像路径：停下或源设备读不到之后，「继续」按同一个源、同一路径续做 */
export interface ImageRequest {
  target: JobTarget
  imagePath: string
}

/** 制作镜像：在通用任务状态之上记下读取进展、结果，以及最近一次发起时的源与路径 */
export function useDiskImage() {
  const imaging = ref<ImagingStatus | null>(null)
  const summary = ref<ImageSummary | null>(null)
  const lastRequest = ref<ImageRequest | null>(null)

  const job = useJob<ImageSummary>({
    onCompleted(result) {
      summary.value = result
    },
    onImaging(status) {
      imaging.value = status
    },
  })

  /** resume 为 true 时按坏区图续做；新建时镜像已存在，宿主返回 imageExists */
  async function start(request: ImageRequest, resume: boolean) {
    if (job.isBusy.value) {
      return
    }
    imaging.value = null
    summary.value = null
    lastRequest.value = request
    await job.start((jobId) => startImage(jobId, request.target, request.imagePath, resume))
  }

  return {
    status: job.status,
    imaging,
    summary,
    lastRequest,
    failure: job.failure,
    isUnresponsive: job.isUnresponsive,
    isBusy: job.isBusy,
    start,
    cancel: job.cancel,
  }
}
