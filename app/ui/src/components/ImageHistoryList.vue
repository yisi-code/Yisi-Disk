<script setup lang="ts">
import { useI18n } from 'vue-i18n'
import type { ImageHistoryItem } from '../services/host'
import { formatBytes, formatDateTime } from '../utils/format'

/**
 * 做过的镜像：位置、来源、现在的状态（做到哪了、上次保存的时间；被删除或移走时说明），
 * 以及「选用」「重新指定位置…」「从列表移除」。只有制作镜像页用
 */
defineProps<{
  items: ImageHistoryItem[]
  disabled: boolean
  /** 正在用的那个镜像，这一行标出来 */
  currentPath: string
}>()
const emit = defineEmits<{
  use: [item: ImageHistoryItem]
  relocate: [item: ImageHistoryItem]
  remove: [item: ImageHistoryItem]
}>()

const { t } = useI18n()

function statusText(item: ImageHistoryItem): string {
  const { status } = item
  if (status.state !== 'ok') {
    return t(`diskImage.history.state.${status.state}`)
  }
  const progress = status.progress
  const size = status.size ?? item.record.size
  if (progress === null) {
    return ''
  }
  const unfinished = progress.untriedBytes + progress.trimBytes + progress.scrapeBytes
  return unfinished === 0
    ? t('diskImage.history.done', { size: formatBytes(size), bad: formatBytes(progress.badBytes) })
    : t('diskImage.history.partial', {
        read: formatBytes(progress.readBytes),
        size: formatBytes(size),
        left: formatBytes(unfinished),
      })
}

function savedText(item: ImageHistoryItem): string {
  const at = item.status.savedAtMs ?? item.record.usedAtMs
  return at > 0 ? t('diskImage.history.savedAt', { time: formatDateTime(new Date(at)) }) : ''
}

function canUse(item: ImageHistoryItem): boolean {
  return item.status.state === 'ok'
}

/** 找不到了、或者只剩其中一个文件：可以重新指定位置 */
function canRelocate(item: ImageHistoryItem): boolean {
  return item.status.state !== 'ok' && item.status.state !== 'mapUnreadable'
}
</script>

<template>
  <ul class="record-list">
    <li
      v-for="item in items"
      :key="item.record.imagePath"
      class="record-list__item"
      :class="{ 'record-list__item--current': item.record.imagePath === currentPath }"
    >
      <div class="record-list__text">
        <span class="record-list__title wizard__path-text">{{ item.record.imagePath }}</span>
        <span class="wizard__note">
          {{ t('diskImage.history.source', { source: item.record.sourceLabel || item.record.sourceIdentity }) }}
          <template v-if="savedText(item)"> · {{ savedText(item) }}</template>
        </span>
        <span :class="canUse(item) ? 'wizard__note' : 'wizard__hint wizard__hint--warning'">
          {{ statusText(item) }}
        </span>
      </div>
      <div class="record-list__actions">
        <button
          v-if="canUse(item)"
          class="wizard__button wizard__button--secondary"
          type="button"
          :disabled="disabled"
          @click="emit('use', item)"
        >
          {{ t('diskImage.history.use') }}
        </button>
        <button
          v-if="canRelocate(item)"
          class="wizard__button wizard__button--secondary"
          type="button"
          :disabled="disabled"
          @click="emit('relocate', item)"
        >
          {{ t('diskImage.history.relocate') }}
        </button>
        <button
          class="wizard__button wizard__button--secondary"
          type="button"
          :disabled="disabled"
          :title="t('diskImage.history.removeHint')"
          @click="emit('remove', item)"
        >
          {{ t('diskImage.history.remove') }}
        </button>
      </div>
    </li>
  </ul>
</template>
