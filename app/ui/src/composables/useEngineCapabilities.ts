import { computed, ref } from 'vue'
import { getEngineCapabilities, toHostError, type EngineCapabilities, type HostError } from '../services/host'

/** 列物理磁盘的方法：只有能打开物理磁盘的平台（目前是 Windows）的引擎才有 */
const LIST_DISKS_METHOD = 'disk.list'

const capabilities = ref<EngineCapabilities | null>(null)
const failure = ref<HostError | null>(null)
let pending: Promise<void> | null = null

/**
 * 引擎能做什么（`engine.capabilities`），全程序共用一份：第一次用到时查一次，之后直接用；关于页每次打开时刷新。
 * 还没查到时一律当作支持，免得界面先灰一下；查不到（引擎起不来）时 `failure` 有值。
 */
export function useEngineCapabilities() {
  /** 查一次（已经在查或查到了就不再查）；`force` 时重新查 */
  function load(force = false): Promise<void> {
    if (pending !== null && !force) {
      return pending
    }
    pending = (async () => {
      try {
        capabilities.value = await getEngineCapabilities()
        failure.value = null
      } catch (error) {
        capabilities.value = null
        failure.value = toHostError(error)
      }
    })()
    return pending
  }

  /** 引擎支持这个方法；还没查到时当作支持，引擎起不来时为假 */
  function supports(method: string): boolean {
    if (failure.value !== null) {
      return false
    }
    return capabilities.value === null || capabilities.value.methods.includes(method)
  }

  /** 能打开物理磁盘与卷（选整块磁盘、盘符、文件夹） */
  const canOpenDevices = computed(() => supports(LIST_DISKS_METHOD))

  return { capabilities, failure, load, supports, canOpenDevices }
}
