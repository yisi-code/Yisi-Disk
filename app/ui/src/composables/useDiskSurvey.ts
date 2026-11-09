import { computed, ref, shallowRef, watch } from 'vue'
import { usePartitionSearch } from './usePartitionSearch'
import type { FeatureKey } from '../navigation'
import {
  diagnoseBoot,
  toHostError,
  type BootDiagnosis,
  type HostError,
  type JobTarget,
  type PartitionBoot,
  type SearchMode,
  type SearchSummary,
} from '../services/host'

/** 一条结论的轻重：要马上处理、要注意、正常、只是说明 */
export type SurveyLevel = 'danger' | 'warning' | 'ok' | 'info'

/** 结论的种类，界面按 survey.finding.<kind> 显示 */
export type SurveyFindingKind =
  | 'readErrors'
  | 'gptDamaged'
  | 'lostPartitions'
  | 'noPartitions'
  | 'healthy'
  | 'backupDamaged'
  | 'primaryDamaged'
  | 'bothDamaged'
  | 'unrecognized'
  | 'ext'
  | 'bitlocker'
  | 'typeMismatch'
  | 'systemPartitions'
  | 'fallback'

export interface SurveyFinding {
  kind: SurveyFindingKind
  level: SurveyLevel
  /** 说的是哪个分区；整块盘的结论为 null */
  partition: PartitionBoot | null
  /** 文案里的数：读不出的字节数、找到的分区数 */
  count: number
  /** 推荐的功能，按先后；转过去时带着同一个源（和这个分区） */
  features: FeatureKey[]
  /** 转到找回丢失的分区时怎么搜 */
  searchMode?: SearchMode
}

export type SurveyPhase = 'idle' | 'diagnosing' | 'searching' | 'done' | 'failed'

/** 没有文件系统也正常的分区用途：保留分区、动态磁盘与存储空间的元数据、交换分区等，不当作「认不出」报出来 */
const SYSTEM_ROLES = [
  'microsoftReserved',
  'ldmMetadata',
  'ldmData',
  'storageSpaces',
  'linuxSwap',
  'linuxLvm',
  'apfs',
  'hfsPlus',
  'gptProtective',
]

/** 引导扇区诊断的结论对应的种类、轻重与推荐功能（ext、BitLocker 另算） */
const VERDICT_FINDINGS: Record<
  'healthy' | 'backupDamaged' | 'primaryDamaged' | 'bothDamaged' | 'unrecognized',
  { level: SurveyLevel; features: FeatureKey[] }
> = {
  healthy: { level: 'ok', features: ['undelete', 'fsCheck'] },
  backupDamaged: { level: 'ok', features: ['undelete', 'repairPartition'] },
  primaryDamaged: { level: 'danger', features: ['repairPartition'] },
  bothDamaged: { level: 'danger', features: ['repairPartition', 'carve'] },
  unrecognized: { level: 'warning', features: ['repairPartition', 'carve'] },
}

/**
 * 「先检查一下这块盘」：只读地读一遍各分区的引导扇区（很快），再做一次快速的丢失分区搜索（几秒），
 * 按结果给出几条结论与推荐的功能。读的过程中出现读错或卡住，结论的第一条就是先做镜像。
 * 不做整盘读取（坏道检测、深度搜索）与文件系统检查：它们耗时长，对坏盘有伤害，只作为推荐
 */
export function useDiskSurvey() {
  const search = usePartitionSearch()
  const target = shallowRef<JobTarget | null>(null)
  const diagnosis = shallowRef<BootDiagnosis | null>(null)
  const phase = ref<SurveyPhase>('idle')
  const failure = ref<HostError | null>(null)
  /** 搜索的过程中出现过读错或卡住（之后恢复了也算） */
  const sawReadTrouble = ref(false)

  watch(search.readHealth, (health) => {
    if (health?.kind === 'damaged' || health?.kind === 'stuck') {
      sawReadTrouble.value = true
    }
  })

  // 搜索结束（搜完、被停下或出错）就出结论；搜索出错时只有引导扇区的结论
  watch(search.status, (status) => {
    if (phase.value === 'searching' && (status === 'completed' || status === 'cancelled' || status === 'failed')) {
      phase.value = 'done'
    }
  })

  const isBusy = computed(
    () => phase.value === 'diagnosing' || phase.value === 'searching' || search.isBusy.value,
  )

  const findings = computed(() =>
    diagnosis.value === null
      ? []
      : surveyFindings(diagnosis.value, search.summary.value, sawReadTrouble.value),
  )

  async function run(source: JobTarget) {
    if (isBusy.value) {
      return
    }
    target.value = source
    diagnosis.value = null
    failure.value = null
    sawReadTrouble.value = false
    phase.value = 'diagnosing'
    try {
      diagnosis.value = await diagnoseBoot(source)
    } catch (error) {
      failure.value = toHostError(error)
      phase.value = 'failed'
      return
    }
    phase.value = 'searching'
    await search.start(source, 'quick')
    // 没能发起搜索（例如引擎起不来）：只给引导扇区的结论
    if (search.status.value === 'failed' || search.status.value === 'idle') {
      phase.value = 'done'
    }
  }

  return { search, target, diagnosis, phase, failure, findings, isBusy, run, stop: search.cancel }
}

/** 由引导扇区诊断与快速搜索的结果归纳结论：整块盘的在前，各分区按位置，最后是兜底的深度恢复 */
export function surveyFindings(
  diagnosis: BootDiagnosis,
  summary: SearchSummary | null,
  sawReadTrouble: boolean,
): SurveyFinding[] {
  const findings: SurveyFinding[] = []
  const unreadable = summary?.unreadableBytes ?? 0
  if (unreadable > 0 || sawReadTrouble) {
    findings.push({ kind: 'readErrors', level: 'danger', partition: null, count: unreadable, features: ['image', 'badSectors'] })
  }
  if (diagnosis.notes.some((note) => note === 'primaryGptInvalid' || note === 'backupGptInvalid')) {
    findings.push({ kind: 'gptDamaged', level: 'warning', partition: null, count: 0, features: ['repairPartition'] })
  }
  const lost = summary?.candidates.filter((candidate) => candidate.relation === 'lost') ?? []
  if (lost.length > 0) {
    findings.push({
      kind: 'lostPartitions',
      level: 'warning',
      partition: null,
      count: lost.length,
      features: ['findPartitions'],
      searchMode: 'quick',
    })
  }
  if (diagnosis.partitions.length === 0 && lost.length === 0) {
    findings.push({
      kind: 'noPartitions',
      level: 'warning',
      partition: null,
      count: 0,
      features: ['findPartitions', 'carve'],
      searchMode: 'deep',
    })
  }

  const systemPartitions: PartitionBoot[] = []
  for (const partition of diagnosis.partitions) {
    if (partition.verdict === 'unrecognized' && partition.role !== null && SYSTEM_ROLES.includes(partition.role)) {
      systemPartitions.push(partition)
      continue
    }
    findings.push(partitionFinding(partition))
    if (partition.tableIssues.some((issue) => issue.code === 'typeMismatch')) {
      findings.push({ kind: 'typeMismatch', level: 'warning', partition, count: 0, features: ['repairPartition'] })
    }
  }
  if (systemPartitions.length > 0) {
    findings.push({
      kind: 'systemPartitions',
      level: 'info',
      partition: null,
      count: systemPartitions.length,
      features: [],
    })
  }
  findings.push({ kind: 'fallback', level: 'info', partition: null, count: 0, features: ['carve'] })
  return findings
}

function partitionFinding(partition: PartitionBoot): SurveyFinding {
  if (partition.verdict === 'unsupported') {
    return partition.filesystem === 'bitlocker'
      ? { kind: 'bitlocker', level: 'info', partition, count: 0, features: [] }
      : { kind: 'ext', level: 'ok', partition, count: 0, features: ['undelete'] }
  }
  const { level, features } = VERDICT_FINDINGS[partition.verdict]
  return { kind: partition.verdict, level, partition, count: 0, features }
}
