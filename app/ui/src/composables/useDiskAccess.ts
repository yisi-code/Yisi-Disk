import { computed, ref } from 'vue'
import {
  authorizeDiskAccess,
  getDiskAccess,
  onDiskAccess,
  toHostError,
  type DiskAccess,
  type HostError,
} from '../services/host'

const access = ref<DiskAccess | null>(null)
const authorizing = ref(false)
const failure = ref<HostError | null>(null)
let started = false

/**
 * 读写物理磁盘的管理员授权，全程序共用一份：第一次用到时问宿主现在的状态，之后跟着宿主的 disk-access 事件更新
 * （任何功能弹了 UAC，结果都会报到这里）。用户取消后，设置页与底栏都可以再点「授权」。
 */
export function useDiskAccess() {
  if (!started) {
    started = true
    void onDiskAccess((value) => {
      access.value = value
    })
    void refresh()
  }

  /** 用户取消过、或代理没能启动：要给出「重新授权」 */
  const needsRetry = computed(() => access.value === 'cancelled' || access.value === 'failed')

  async function refresh() {
    try {
      access.value = await getDiskAccess()
    } catch (error) {
      failure.value = toHostError(error)
    }
  }

  /** 现在就请求授权（弹 UAC）；取消时状态变成 cancelled，可以再点 */
  async function authorize() {
    if (authorizing.value) {
      return
    }
    authorizing.value = true
    failure.value = null
    try {
      access.value = await authorizeDiskAccess()
    } catch (error) {
      const hostError = toHostError(error)
      // 取消已经由状态表示，不再另报错误
      failure.value = hostError.code === 'elevationCancelled' ? null : hostError
      await refresh()
    } finally {
      authorizing.value = false
    }
  }

  return { access, authorizing, failure, needsRetry, authorize }
}
