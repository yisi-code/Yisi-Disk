import { useI18n } from 'vue-i18n'
import { usePartitionText } from './usePartitionText'
import type { PartitionTableDetail, PlanSummary, WriteRecord } from '../services/host'
import { formatBytes } from '../utils/format'

/** 写入计划与写盘记录的文字：做了什么、结果怎样。确认页、写盘记录、执行后的提示共用 */
export function useWritePlanText() {
  const { t, te } = useI18n()
  const { filesystemName } = usePartitionText()

  /** 「分区 N：用备份修复主引导扇区」「把找到的分区写回分区表…」；回滚时套上「回滚（…）」 */
  function writeActionText(summary: PlanSummary): string {
    const action = baseActionText(summary)
    return summary.kind === 'rollback' ? t('writePlan.action.rollback', { action }) : action
  }

  function baseActionText(summary: PlanSummary): string {
    const boot = summary.bootSector
    if (boot !== null) {
      return t(`writePlan.action.${boot.fix}`, { number: boot.partition })
    }
    const table = summary.partitionTable
    if (table === null) {
      return t('writePlan.action.unknown')
    }
    return table.fix.kind === 'addLost'
      ? t('writePlan.action.addLost', {
          filesystem: filesystemName(table.filesystem),
          start: formatBytes(table.offset),
          size: formatBytes(table.length),
        })
      : t('writePlan.action.setType', { number: table.partition, type: tableTypeText(table) })
  }

  /** 改完之后的分区类型：常见的给出用途，后面附类型字节或 GUID */
  function tableTypeText(table: PartitionTableDetail): string {
    const key = `partition.role.${table.role ?? ''}`
    return table.role !== null && te(key)
      ? t('writePlan.typeValue', { role: t(key), type: table.typeId })
      : table.typeId
  }

  /** 写成了、一个字节没写、写到一半三种说法；失败时说明停在哪一步 */
  function writeOutcomeText(record: WriteRecord): string {
    const { outcome } = record
    if (outcome.done) {
      return t('writePlan.outcome.done')
    }
    const step = outcome.step === null ? '' : t(`writePlan.step.${outcome.step}`)
    return outcome.written === 0
      ? t('writePlan.outcome.untouched', { step })
      : t('writePlan.outcome.partial', { step, written: outcome.written })
  }

  return { writeActionText, writeOutcomeText, tableTypeText }
}
