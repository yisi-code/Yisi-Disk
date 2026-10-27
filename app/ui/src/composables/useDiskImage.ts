import { ref } from 'vue'
import { useJob } from './useJob'
import {
  startImage,
  type ImagePlan,
  type ImageSummary,
  type ImagingStatus,
  type JobTarget,
} from '../services/host'

/** 一次制作镜像的源、镜像路径与做法：停下或源设备读不到之后，「继续」按同一个源、同一路径、同样的范围续做 */
export interface ImageRequest {
  target: JobTarget
  imagePath: string
  plan: ImagePlan
  /** 给人看的来源描述，记进做过的镜像列表 */
  sourceLabel: string
}

/** 制作镜像：在通用任务状态之上记下读取进展、结果，以及最近一次发起时的源、路径与做法 */
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
    await job.start((jobId) =>
      startImage(jobId, request.target, request.imagePath, resume, request.plan, request.sourceLabel),
    )
  }

  /** 接着做最近一次：同样的范围，但不再清掉进度（要清的上次开始时已经清过） */
  async function continueLast() {
    const last = lastRequest.value
    if (last !== null) {
      await start({ ...last, plan: { ...last.plan, reset: [] } }, true)
    }
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
    continueLast,
    cancel: job.cancel,
  }
}
