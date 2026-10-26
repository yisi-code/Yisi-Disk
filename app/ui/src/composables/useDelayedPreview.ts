import { onBeforeUnmount, ref, shallowRef, watch } from 'vue'
import { toHostError, type FilePreview, type HostError } from '../services/host'

/** 选中后等这么久再请引擎读取：用方向键快速移动时，不为途经的每个文件都读一次 */
const PREVIEW_DELAY_MS = 150

/**
 * 跟随当前选中的对象读取预览。`target` 返回要预览的对象（没有时为 null），`keyOf` 给出它的标识，
 * 标识变了才重新读；`load` 向宿主要预览。每次换选中都作废旧请求，晚到的旧答复丢弃。
 */
export function useDelayedPreview<TTarget>(
  target: () => TTarget | null,
  keyOf: (value: TTarget) => string,
  load: (value: TTarget) => Promise<FilePreview>,
) {
  const preview = shallowRef<FilePreview | null>(null)
  const error = ref<HostError | null>(null)
  const isLoading = ref(false)

  let pendingTimer: ReturnType<typeof setTimeout> | undefined
  let requestSerial = 0

  watch(
    () => {
      const current = target()
      return current === null ? null : keyOf(current)
    },
    () => {
      clearTimeout(pendingTimer)
      requestSerial += 1
      preview.value = null
      error.value = null
      const current = target()
      if (current === null) {
        isLoading.value = false
        return
      }
      isLoading.value = true
      const serial = requestSerial
      pendingTimer = setTimeout(() => void fetchPreview(current, serial), PREVIEW_DELAY_MS)
    },
    { immediate: true },
  )

  onBeforeUnmount(() => {
    clearTimeout(pendingTimer)
    requestSerial += 1
  })

  async function fetchPreview(current: TTarget, serial: number) {
    try {
      const result = await load(current)
      if (serial === requestSerial) {
        preview.value = result
      }
    } catch (caught) {
      if (serial === requestSerial) {
        error.value = toHostError(caught)
      }
    } finally {
      if (serial === requestSerial) {
        isLoading.value = false
      }
    }
  }

  return { preview, error, isLoading }
}
