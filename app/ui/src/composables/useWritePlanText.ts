import { useI18n } from 'vue-i18n'
import type { PlanSummary, WriteRecord } from '../services/host'

/** 写入计划与写盘记录的文字：做了什么、结果怎样。确认页、写盘记录、执行后的提示共用 */
export function useWritePlanText() {
  const { t } = useI18n()

  /** 「分区 N：用备份修复主引导扇区」；回滚时套上「回滚（…）」 */
  function writeActionText(summary: PlanSummary): string {
    const detail = summary.bootSector
    const action =
      detail === null
        ? t('writePlan.action.unknown')
        : t(`writePlan.action.${detail.fix}`, { number: detail.partition })
    return summary.kind === 'rollback' ? t('writePlan.action.rollback', { action }) : action
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

  return { writeActionText, writeOutcomeText }
}
