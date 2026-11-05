import { computed, ref, shallowRef, toValue, type MaybeRefOrGetter } from 'vue'
import {
  discardWritePlan,
  executeWritePlan,
  listWriteHistory,
  planBootSector,
  planFsFix,
  planMft,
  planPartitionTable,
  planRollback,
  sameTarget,
  toHostError,
  type BootFix,
  type ClusterRun,
  type FsFix,
  type HostError,
  type JobTarget,
  type MftFix,
  type PartitionRef,
  type PartitionTableFix,
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
  /** 当前查看的源上的写盘记录（宿主按认盘标识筛好的），以及这块盘是不是查不到序列号 */
  const records = ref<WriteRecord[]>([])
  const indistinct = ref(false)
  /** 写盘记录是哪个源的：执行之后按它重新读 */
  let historyTarget: JobTarget | null = null
  const isPreparing = ref(false)
  const isExecuting = ref(false)
  /** 生成、执行、放弃计划出的错 */
  const failure = ref<HostError | null>(null)

  /** 读这个源的写盘记录；没有源时清空 */
  async function loadRecords(target: JobTarget | null = historyTarget) {
    historyTarget = target
    if (target === null) {
      records.value = []
      indistinct.value = false
      return
    }
    try {
      const history = await listWriteHistory(target)
      // 读的过程中换了源：这份结果作废
      if (historyTarget === target) {
        records.value = history.records
        indistinct.value = history.indistinct
      }
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

  function prepareTableFix(target: JobTarget, fix: PartitionTableFix): Promise<boolean> {
    return prepare(() => planPartitionTable(target, fix))
  }

  function prepareMftFix(target: JobTarget, partition: PartitionRef, fix: MftFix): Promise<boolean> {
    return prepare(() => planMft(target, partition, fix))
  }

  function prepareFsFix(
    target: JobTarget,
    partition: PartitionRef,
    fixes: FsFix[],
    badClusters: ClusterRun[] = [],
  ): Promise<boolean> {
    return prepare(() => planFsFix(target, partition, fixes, badClusters))
  }

  /** 回滚写盘记录里的一次写入；target 是现在查看的源（宿主核对它就是当时写的那块盘） */
  function prepareRollback(planId: string, target: JobTarget): Promise<boolean> {
    return prepare(() => planRollback(planId, target))
  }

  /** 执行等确认的计划。写之前被拒绝（镜像或卷正被占用等）时宿主留着计划，这里也留着，处理之后可以再试 */
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
      if (failure.value.code === 'planNotFound') {
        plan.value = null
      }
      return null
    } finally {
      isExecuting.value = false
    }
  }

  /** 刚执行的那次写入：只有同一个源时才给出（换了源不显示上一次的结果） */
  function lastRecordFor(target: MaybeRefOrGetter<JobTarget | null>) {
    return computed(() => {
      const record = lastRecord.value
      return record !== null && sameTarget(record.target, toValue(target)) ? record : null
    })
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
    indistinct,
    isPreparing,
    isExecuting,
    failure,
    loadRecords,
    lastRecordFor,
    prepareBootSector,
    prepareTableFix,
    prepareMftFix,
    prepareFsFix,
    prepareRollback,
    execute,
    discard,
  }
}
