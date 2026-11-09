import { inject, provide, shallowRef, type InjectionKey } from 'vue'
import { rememberSource } from './useSourceSelection'
import type { FeatureKey } from '../navigation'
import type { JobTarget, PartitionRef, SearchMode } from '../services/host'

/**
 * 从一个功能直接转到另一个功能时带过去的东西：同一个源，可选的同一个分区；转到找回丢失的分区时可以指定怎么搜
 * （quick 进来就搜，deep 只是先选好、等用户点开始——深度搜索要读一遍整块盘）
 */
export interface FeatureHandoff {
  feature: FeatureKey
  target: JobTarget
  partition: PartitionRef | null
  searchMode?: SearchMode
}

export type FeatureJump = (handoff: FeatureHandoff) => Promise<void>

const JUMP_KEY: InjectionKey<FeatureJump> = Symbol('featureJump')

/** 某个功能页要求转过去、正在等它收尾退出 */
const requested = shallowRef<FeatureHandoff | null>(null)
/** 已经转过去、等新的功能页取走 */
const incoming = shallowRef<FeatureHandoff | null>(null)

/**
 * 功能页用：转到另一个功能。先把源记成最近选过的，再走这个功能页自己的退出（关浏览、放弃没确认的计划）；
 * 退出被拒（还有任务在跑）时不转。功能页里的子组件经 `injectFeatureJump` 拿到同一个入口
 */
export function useFeatureJump(exit: () => void | Promise<void>): FeatureJump {
  async function jump(handoff: FeatureHandoff) {
    rememberSource(handoff.target)
    requested.value = handoff
    await exit()
    // 退出时宿主（App）会取走；没取走说明退出被拒
    if (requested.value === handoff) {
      requested.value = null
    }
  }
  provide(JUMP_KEY, jump)
  return jump
}

/** 子组件用：所在的功能页能转到别的功能时才有 */
export function injectFeatureJump(): FeatureJump | null {
  return inject(JUMP_KEY, null)
}

/** App 用：功能页退出时，有没有要接着打开的功能；有就交给新的功能页 */
export function acceptFeatureJump(): FeatureKey | null {
  const handoff = requested.value
  requested.value = null
  incoming.value = handoff
  return handoff?.feature ?? null
}

/** 功能页打开时用：是不是从别的功能转过来的，带着什么 */
export function takeFeatureHandoff(feature: FeatureKey): FeatureHandoff | null {
  const handoff = incoming.value
  if (handoff === null || handoff.feature !== feature) {
    return null
  }
  incoming.value = null
  return handoff
}
