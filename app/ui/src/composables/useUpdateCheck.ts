import { ref } from 'vue'
import {
  checkUpdate,
  installUpdate,
  onUpdateProgress,
  toHostError,
  type HostError,
  type UpdateProgress,
} from '../services/host'

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

/** 两步各自的进度：① 下载（字节）、② 安装（核对 → 解压 → 换上，件数）；还没开始的为 null */
export interface UpdateSteps {
  download: { done: number; total: number; source: string } | null
  install: { step: 'verify' | 'extract' | 'replace'; done: number; total: number } | null
}

/** 上次自动检查成功的时间（毫秒），存在 WebView 的 localStorage 里（数据在程序文件夹的 data 下） */
const AUTO_CHECK_KEY = 'yisi.updateCheckedAt'
/** 自动检查最多一天一次 */
const AUTO_CHECK_INTERVAL_MS = 24 * 60 * 60 * 1000

// 全程序共用一份：启动后自动检查、弹窗里的「立即更新」、关于页显示的都是同一个状态，离开关于页再回来进度还在
const state = ref<UpdateState>({ kind: 'idle' })
/** 点了「下载并更新」之后才有；失败时保留，看得出停在哪一步 */
const steps = ref<UpdateSteps | null>(null)
/** 当前选用的发布源：检查时取答复最快的，下载中途换源时跟着变；打开下载页也用它 */
const source = ref<string | null>(null)

function autoCheckDue(): boolean {
  try {
    const last = Number(localStorage.getItem(AUTO_CHECK_KEY))
    return !Number.isFinite(last) || Date.now() - last >= AUTO_CHECK_INTERVAL_MS || last > Date.now()
  } catch {
    return true
  }
}

function markAutoChecked() {
  try {
    localStorage.setItem(AUTO_CHECK_KEY, String(Date.now()))
  } catch {
    // 存不下只是下次多查一次
  }
}

/**
 * 检查与安装新版本：由宿主联网（问 GitHub 与 Gitee 镜像的最新发布、选快的下载、核对签名与摘要、换上、重启）。
 * 打开程序后自动查一次（一天最多一次，见 autoCheck），也可以在关于页手动查；下载与安装只在用户点了之后才做。
 * 版本号的比较、签名核对都在宿主里，界面只显示结果与宿主报来的两步进度。
 */
export function useUpdateCheck() {
  async function check() {
    state.value = { kind: 'checking' }
    try {
      const info = await checkUpdate()
      source.value = info.source
      state.value = info.available
        ? { kind: 'available', latest: info.latest }
        : { kind: 'latest', latest: info.latest }
    } catch (error) {
      state.value = { kind: 'failed', error: toHostError(error), latest: null }
    }
  }

  function applyProgress(progress: UpdateProgress) {
    if (steps.value === null) {
      return
    }
    source.value = progress.source
    if (progress.phase === 'download') {
      steps.value.download = { done: progress.done, total: progress.total, source: progress.source }
    } else if (progress.step === 'verify' || progress.step === 'extract' || progress.step === 'replace') {
      steps.value.install = { step: progress.step, done: progress.done, total: progress.total }
    }
  }

  async function install(latest: string) {
    state.value = { kind: 'installing', latest }
    steps.value = { download: null, install: null }
    // 先监听再开始：宿主一开始就会报下载进度
    const unlisten = await onUpdateProgress(applyProgress)
    try {
      const version = await installUpdate()
      state.value = { kind: 'restarting', version }
    } catch (error) {
      state.value = { kind: 'failed', error: toHostError(error), latest }
    } finally {
      unlisten()
    }
  }

  /**
   * 自动检查：距上次成功检查不到一天、或已经在查 / 在装时不查；查不到（没联网等）时不报错，回到 idle，
   * 手动检查才显示原因。有新版本时返回它的版本号，供弹窗提示
   */
  async function autoCheck(): Promise<string | null> {
    if (state.value.kind !== 'idle' || !autoCheckDue()) {
      return null
    }
    state.value = { kind: 'checking' }
    try {
      const info = await checkUpdate()
      markAutoChecked()
      source.value = info.source
      state.value = info.available
        ? { kind: 'available', latest: info.latest }
        : { kind: 'latest', latest: info.latest }
      return info.available ? info.latest : null
    } catch {
      state.value = { kind: 'idle' }
      return null
    }
  }

  return { state, steps, source, check, install, autoCheck }
}
