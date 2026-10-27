import { ref } from 'vue'
import {
  listImageHistory,
  relocateImageHistory,
  removeImageHistory,
  toHostError,
  type HostError,
  type ImageHistoryItem,
} from '../services/host'

/**
 * 做过的镜像：列表（各带现在的状态）、从列表移除（不删文件）、镜像被移走后重新指定位置。
 * 只有制作镜像页用；宿主每次开始做镜像时自动记下
 */
export function useImageHistory() {
  const items = ref<ImageHistoryItem[]>([])
  const isLoading = ref(false)
  const loadError = ref<HostError | null>(null)
  /** 移除、重新指定位置出的错，带上是哪一条 */
  const actionError = ref<{ imagePath: string; error: HostError } | null>(null)

  async function load() {
    isLoading.value = true
    loadError.value = null
    try {
      items.value = await listImageHistory()
    } catch (error) {
      loadError.value = toHostError(error)
    } finally {
      isLoading.value = false
    }
  }

  async function remove(imagePath: string) {
    actionError.value = null
    try {
      await removeImageHistory(imagePath)
      items.value = items.value.filter((item) => item.record.imagePath !== imagePath)
    } catch (error) {
      actionError.value = { imagePath, error: toHostError(error) }
    }
  }

  /** 改成新位置；成功时返回更新后的那一条 */
  async function relocate(imagePath: string, newPath: string): Promise<ImageHistoryItem | null> {
    actionError.value = null
    try {
      const moved = await relocateImageHistory(imagePath, newPath)
      await load()
      return moved
    } catch (error) {
      actionError.value = { imagePath, error: toHostError(error) }
      return null
    }
  }

  return { items, isLoading, loadError, actionError, load, remove, relocate }
}
