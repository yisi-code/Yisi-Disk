<script setup lang="ts">
import { useI18n } from 'vue-i18n'
import { useWritePlanText } from '../composables/useWritePlanText'
import type { WriteRecord } from '../services/host'
import { formatDateTime } from '../utils/format'

/**
 * 写盘记录：什么时候做了什么、结果怎样、备份在哪；写下去过内容、还没回滚的修复可以「回滚…」（同样要确认）。
 * 记录最新的在前，回滚本身也是一条记录
 */
const props = defineProps<{
  records: WriteRecord[]
  disabled: boolean
}>()
const emit = defineEmits<{ rollback: [record: WriteRecord] }>()

const { t } = useI18n()
const { writeActionText, writeOutcomeText } = useWritePlanText()

function isRolledBack(record: WriteRecord): boolean {
  return props.records.some((other) => other.rollbackOf === record.planId && other.outcome.done)
}

/** 回滚本身不再回滚；一个字节没写的不用回滚 */
function canRollback(record: WriteRecord): boolean {
  return record.kind !== 'rollback' && record.outcome.written > 0 && !isRolledBack(record)
}
</script>

<template>
  <ul class="record-list">
    <li v-for="record in records" :key="record.planId" class="record-list__item">
      <div class="record-list__text">
        <span class="record-list__title">{{ writeActionText(record) }}</span>
        <span class="wizard__note">{{ formatDateTime(new Date(record.executedAtMs)) }}</span>
        <span :class="record.outcome.done ? 'wizard__note' : 'wizard__hint wizard__hint--warning'">
          {{ writeOutcomeText(record) }}
        </span>
        <span class="wizard__note wizard__path-text">
          {{ t('writePlan.history.backup', { path: record.backupPath }) }}
        </span>
      </div>
      <div class="record-list__actions">
        <button
          v-if="canRollback(record)"
          class="wizard__button wizard__button--secondary"
          type="button"
          :disabled="disabled"
          @click="emit('rollback', record)"
        >
          {{ t('writePlan.history.rollback') }}
        </button>
        <span v-else-if="isRolledBack(record)" class="wizard__note">{{ t('writePlan.history.rolledBack') }}</span>
      </div>
    </li>
  </ul>
</template>
