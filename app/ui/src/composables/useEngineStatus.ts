import { computed, reactive, ref } from 'vue'
import { getEngineCapabilities, toHostError, type HostError } from '../services/host'

/**
 * 引擎的状态，按要紧程度从低到高：
 * - checking：程序刚启动，还在确认引擎在不在
 * - idle：引擎就绪，没有任务
 * - session：浏览会话开着（已打开源，等用户操作）
 * - paused：任务暂停中
 * - running：任务运行中
 * - starting：正在启动引擎（物理盘可能在等管理员授权）
 * - cancelling：正在停止
 * - unresponsive：引擎超过 10 秒没有心跳
 * - unavailable：引擎启动不了（程序缺失等）
 */
export type EngineState =
  | 'checking'
  | 'idle'
  | 'session'
  | 'paused'
  | 'running'
  | 'starting'
  | 'cancelling'
  | 'unresponsive'
  | 'unavailable'

const RANK: Record<EngineState, number> = {
  checking: 0,
  idle: 1,
  session: 2,
  paused: 3,
  running: 4,
  starting: 5,
  cancelling: 6,
  unresponsive: 7,
  unavailable: 8,
}

/** 各个任务（useJob）报上来的状态；任务结束或页面关掉时撤回 */
const reported = reactive(new Map<symbol, EngineState>())
const availability = ref<EngineState>('checking')
const availabilityError = ref<HostError | null>(null)
let checked = false

/** useJob 报告自己的状态；null 表示没有任务在跑（撤回） */
export function reportJobState(key: symbol, state: EngineState | null) {
  if (state === null) {
    reported.delete(key)
    return
  }
  reported.set(key, state)
  // 任务起来了，引擎显然在：启动时那次没问到（例如刚好超时）也不再显示不可用
  if (state !== 'starting' && availability.value === 'unavailable') {
    availability.value = 'idle'
    availabilityError.value = null
  }
}

/** 底栏用：引擎在不在（启动时问一次），以及各任务里最要紧的那个状态 */
export function useEngineStatus() {
  if (!checked) {
    checked = true
    void getEngineCapabilities()
      .then(() => {
        availability.value = 'idle'
      })
      .catch((error: unknown) => {
        availability.value = 'unavailable'
        availabilityError.value = toHostError(error)
      })
  }

  // 有任务时看任务里最要紧的那个，没有任务时看引擎在不在
  const state = computed<EngineState>(() =>
    [...reported.values()].reduce<EngineState | null>(
      (current, jobState) => (current === null || RANK[jobState] > RANK[current] ? jobState : current),
      null,
    ) ?? availability.value,
  )

  return { state, availabilityError }
}
