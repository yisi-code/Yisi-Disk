import { ref } from 'vue'
import { checkUpdate, installUpdate, toHostError, type HostError } from '../services/host'

export type UpdateState =
  | { kind: 'idle' }
  | { kind: 'checking' }
  | { kind: 'latest'; latest: string }
  | { kind: 'available'; latest: string }
  | { kind: 'installing'; latest: string }
  /** 新版本已换上，宿主马上重启程序 */
  | { kind: 'restarting'; version: string }
  /** latest 有值时表示已经查到了新版本（下载或换上时失败），仍可以手动去下载页 */
  | { kind: 'failed'; error: HostError; latest: string | null }

/**
 * 检查与安装新版本：只在用户点按时由宿主联网（问 GitHub 的最新发布、下载、核对签名与摘要、换上、重启）。
 * 版本号的比较、签名核对都在宿主里，界面只显示结果。
 */
export function useUpdateCheck() {
  const state = ref<UpdateState>({ kind: 'idle' })

  async function check() {
    state.value = { kind: 'checking' }
    try {
      const info = await checkUpdate()
      state.value = info.available
        ? { kind: 'available', latest: info.latest }
        : { kind: 'latest', latest: info.latest }
    } catch (error) {
      state.value = { kind: 'failed', error: toHostError(error), latest: null }
    }
  }

  async function install(latest: string) {
    state.value = { kind: 'installing', latest }
    try {
      const version = await installUpdate()
      state.value = { kind: 'restarting', version }
    } catch (error) {
      state.value = { kind: 'failed', error: toHostError(error), latest }
    }
  }

  return { state, check, install }
}
