import { ref } from 'vue'
import {
  inspectImage,
  toHostError,
  type HostError,
  type ImageInspection,
  type JobTarget,
} from '../services/host'

/**
 * 做镜像之前看一眼源与镜像位置（源上的分区、输出位置、已有镜像与源的变化）。
 * 连着改源或路径时只认最后一次的答复，前面的答复晚到也不会盖掉它
 */
export function useImageInspection() {
  const inspection = ref<ImageInspection | null>(null)
  const isInspecting = ref(false)
  const failure = ref<HostError | null>(null)
  /** 这一份结果是对哪个源、哪个路径看的 */
  const inspected = ref<{ target: JobTarget; imagePath: string } | null>(null)
  let latest = 0

  async function inspect(target: JobTarget, imagePath: string) {
    const request = ++latest
    isInspecting.value = true
    failure.value = null
    try {
      const result = await inspectImage(target, imagePath)
      if (request === latest) {
        inspection.value = result
        inspected.value = { target, imagePath }
      }
    } catch (error) {
      if (request === latest) {
        inspection.value = null
        inspected.value = null
        failure.value = toHostError(error)
      }
    } finally {
      if (request === latest) {
        isInspecting.value = false
      }
    }
  }

  /** 源或路径变了：作废手上的结果（还在路上的答复也不要了） */
  function clear() {
    latest += 1
    inspection.value = null
    inspected.value = null
    failure.value = null
    isInspecting.value = false
  }

  return { inspection, inspected, isInspecting, failure, inspect, clear }
}
