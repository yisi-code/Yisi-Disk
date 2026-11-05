<script setup lang="ts">
import { useI18n } from 'vue-i18n'
import type { CarveRecord } from '../services/host'
import { formatBytes, formatDateTime } from '../utils/format'

/**
 * 深度恢复的扫描记录：来源、怎么保存、开始的时间、扫到哪了、找到多少个文件，
 * 以及「查看结果」（接着挑选保存）、「接着扫」（停下或出错的）、「删除记录」。只有深度恢复页用
 */
defineProps<{
  items: CarveRecord[]
  disabled: boolean
}>()
const emit = defineEmits<{
  open: [record: CarveRecord]
  resume: [record: CarveRecord]
  remove: [record: CarveRecord]
}>()

const { t } = useI18n()

function percent(record: CarveRecord): number {
  return record.size > 0 ? Math.floor((record.resumeAt / record.size) * 100) : 0
}

function statusText(record: CarveRecord): string {
  return t(`carve.history.status.${record.status}`, {
    percent: percent(record),
    position: formatBytes(record.resumeAt),
    size: formatBytes(record.size),
    count: record.files,
  }, record.files)
}

function canResume(record: CarveRecord): boolean {
  return record.status === 'stopped' || record.status === 'failed'
}
</script>

<template>
  <ul class="record-list">
    <li v-for="record in items" :key="record.id" class="record-list__item">
      <div class="record-list__text">
        <span class="record-list__title wizard__path-text">{{ record.sourceLabel }}</span>
        <span class="wizard__note">
          {{
            record.outputDir === null
              ? t('carve.history.scanOnly')
              : t('carve.history.savedTo', { dir: record.outputDir })
          }}
          · {{ t('carve.history.startedAt', { time: formatDateTime(new Date(record.startedAtMs)) }) }}
        </span>
        <span :class="record.status === 'failed' ? 'wizard__hint wizard__hint--warning' : 'wizard__note'">
          {{ statusText(record) }}
        </span>
      </div>
      <div class="record-list__actions">
        <button
          class="wizard__button wizard__button--secondary"
          type="button"
          :disabled="disabled || record.status === 'running'"
          @click="emit('open', record)"
        >
          {{ t('carve.history.open') }}
        </button>
        <button
          v-if="canResume(record)"
          class="wizard__button wizard__button--secondary"
          type="button"
          :disabled="disabled"
          @click="emit('resume', record)"
        >
          {{ t('carve.history.resume') }}
        </button>
        <button
          class="wizard__button wizard__button--secondary"
          type="button"
          :disabled="disabled || record.status === 'running'"
          @click="emit('remove', record)"
        >
          {{ t('carve.history.remove') }}
        </button>
      </div>
    </li>
  </ul>
</template>
