import { useI18n } from 'vue-i18n'
import { usePartitionText } from './usePartitionText'
import type { PartitionTableDetail, PlanSummary, WriteRecord } from '../services/host'
import { formatBytes } from '../utils/format'

/** 写入计划与写盘记录的文字：做了什么、结果怎样。确认页、写盘记录、执行后的提示共用 */
export function useWritePlanText() {
  const { t } = useI18n()
  const { filesystemName, sourceName, roleName } = usePartitionText()

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
    const mft = summary.mft
    if (mft !== null) {
      return t(`writePlan.action.${mft.fix}`, { number: mft.partition })
    }
    const fsFix = summary.fsFix
    if (fsFix !== null) {
      return t('writePlan.action.fsFix', { number: fsFix.partition })
    }
    const table = summary.partitionTable
    if (table === null) {
      return t('writePlan.action.unknown')
    }
    if (table.fix.kind === 'repairGpt') {
      return t(`writePlan.action.repairGpt.${table.gptRepair ?? 'primaryFromBackup'}`)
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
    return roleName(table, 'always')
  }

  /**
   * 写到哪里：物理盘是「磁盘 3（型号，容量，序列号 …）」，查不到序列号时写明；编号是写的时候的。镜像文件是它的路径
   */
  function writeTargetText(summary: PlanSummary): string {
    const name = sourceName(summary.target)
    const device = summary.device
    if (summary.target.kind !== 'disk' || device === null) {
      return name
    }
    const model = device.model === '' ? t('writePlan.device.unknownModel') : device.model
    const size = formatBytes(device.size)
    const detail =
      device.serial === ''
        ? t('writePlan.device.noSerial', { model, size })
        : t('writePlan.device.withSerial', { model, size, serial: device.serial })
    return t('writePlan.device.named', { name, detail })
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

  return { writeActionText, writeOutcomeText, writeTargetText, tableTypeText }
}
