import { ref } from 'vue'

/** GitHub 上本项目最新发布的查询地址；界面安全策略只放行这个主机 */
const LATEST_RELEASE_API = 'https://api.github.com/repos/yisi-code/Yisi-Disk/releases/latest'
/** 网络不通时最多等这么久，不让按钮一直停在「正在检查」 */
const CHECK_TIMEOUT_MS = 20_000

export type UpdateState =
  | { kind: 'idle' }
  | { kind: 'checking' }
  | { kind: 'latest'; latest: string }
  | { kind: 'available'; latest: string }
  /** reason 为 null 表示等太久没有答复 */
  | { kind: 'failed'; reason: string | null }

/**
 * 比较两个版本号（`v0.1.1`、`0.2.0` 这类，只比数字段）：a 比 b 新为正数，相同为 0。
 * 预发布后缀（`-beta` 等）不参与比较。
 */
export function compareVersions(a: string, b: string): number {
  const parts = (version: string) =>
    version
      .replace(/^v/i, '')
      .split('-')[0]
      .split('.')
      .map((part) => Number.parseInt(part, 10) || 0)
  const left = parts(a)
  const right = parts(b)
  for (let index = 0; index < Math.max(left.length, right.length); index += 1) {
    const difference = (left[index] ?? 0) - (right[index] ?? 0)
    if (difference !== 0) {
      return difference
    }
  }
  return 0
}

/**
 * 检查新版本：只在用户点按时联网问一次 GitHub 的最新发布，不自动检查、不下载。
 * 下载交给浏览器打开发布页（宿主 `open_releases_page`）。
 */
export function useUpdateCheck() {
  const state = ref<UpdateState>({ kind: 'idle' })

  async function check(current: string) {
    state.value = { kind: 'checking' }
    const abort = new AbortController()
    const timer = setTimeout(() => abort.abort(), CHECK_TIMEOUT_MS)
    try {
      const response = await fetch(LATEST_RELEASE_API, {
        headers: { Accept: 'application/vnd.github+json' },
        signal: abort.signal,
      })
      if (!response.ok) {
        state.value = { kind: 'failed', reason: `HTTP ${response.status}` }
        return
      }
      const release = (await response.json()) as { tag_name?: unknown }
      if (typeof release.tag_name !== 'string' || release.tag_name === '') {
        state.value = { kind: 'failed', reason: 'tag_name' }
        return
      }
      const latest = release.tag_name.replace(/^v/i, '')
      state.value =
        compareVersions(latest, current) > 0 ? { kind: 'available', latest } : { kind: 'latest', latest }
    } catch (error) {
      const reason = abort.signal.aborted ? null : error instanceof Error ? error.message : String(error)
      state.value = { kind: 'failed', reason }
    } finally {
      clearTimeout(timer)
    }
  }

  return { state, check }
}
