import { ref, shallowRef } from 'vue'
import {
  discardWritePlan,
  executeWritePlan,
  listWriteHistory,
  planBootSector,
  planRollback,
  toHostError,
  type BootFix,
  type HostError,
  type JobTarget,
  type PlanView,
  type WriteRecord,
} from '../services/host'

/**
 * 写盘的流程：生成计划（宿主先存好备份）→ 界面确认 → 执行 → 记进写盘记录；回滚同样先生成计划、确认后执行。
 * 计划一次只有一份；确认前什么都不写
 */
export function useWritePlan() {
  /** 等确认的计划 */
  const plan = shallowRef<PlanView | null>(null)
  /** 最近一次执行的结果 */
  const lastRecord = shallowRef<WriteRecord | null>(null)
  const records = ref<WriteRecord[]>([])
  const isPreparing = ref(false)
  const isExecuting = ref(false)
  /** 生成、执行、放弃计划出的错 */
  const failure = ref<HostError | null>(null)

  async function loadRecords() {
    try {
      records.value = await listWriteHistory()
    } catch (error) {
      failure.value = toHostError(error)
    }
  }

  async function prepare(create: () => Promise<PlanView>): Promise<boolean> {
    if (isPreparing.value || isExecuting.value) {
      return false
    }
    await discard()
    failure.value = null
    isPreparing.value = true
    try {
      plan.value = await create()
      return true
    } catch (error) {
      failure.value = toHostError(error)
      return false
    } finally {
      isPreparing.value = false
    }
  }

  function prepareBootSector(target: JobTarget, partition: number, fix: BootFix): Promise<boolean> {
    return prepare(() => planBootSector(target, partition, fix))
  }

  function prepareRollback(planId: string): Promise<boolean> {
    return prepare(() => planRollback(planId))
  }

  /** 执行等确认的计划。镜像正被占用（imageInUse）时计划留着，可以关掉占用的程序再试 */
  async function execute(): Promise<WriteRecord | null> {
    const current = plan.value
    if (current === null || isExecuting.value) {
      return null
    }
    failure.value = null
    isExecuting.value = true
    try {
      const record = await executeWritePlan(current.planId)
      plan.value = null
      lastRecord.value = record
      await loadRecords()
      return record
    } catch (error) {
      failure.value = toHostError(error)
      if (failure.value.code !== 'imageInUse') {
        plan.value = null
      }
      return null
    } finally {
      isExecuting.value = false
    }
  }

  /** 放弃等确认的计划（宿主删掉它的备份） */
  async function discard() {
    const current = plan.value
    plan.value = null
    if (current === null) {
      return
    }
    try {
      await discardWritePlan(current.planId)
    } catch (error) {
      failure.value = toHostError(error)
    }
  }

  return {
    plan,
    lastRecord,
    records,
    isPreparing,
    isExecuting,
    failure,
    loadRecords,
    prepareBootSector,
    prepareRollback,
    execute,
    discard,
  }
}
