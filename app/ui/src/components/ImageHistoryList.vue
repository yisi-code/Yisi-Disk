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
  <ul class="image-history">
    <li
      v-for="item in items"
      :key="item.record.imagePath"
      class="image-history__item"
      :class="{ 'image-history__item--current': item.record.imagePath === currentPath }"
    >
      <div class="image-history__text">
        <span class="image-history__path wizard__path-text">{{ item.record.imagePath }}</span>
        <span class="wizard__note">
          {{ t('diskImage.history.source', { source: item.record.sourceLabel || item.record.sourceIdentity }) }}
          <template v-if="savedText(item)"> · {{ savedText(item) }}</template>
        </span>
        <span :class="canUse(item) ? 'wizard__note' : 'wizard__hint wizard__hint--warning'">
          {{ statusText(item) }}
        </span>
      </div>
      <div class="image-history__actions">
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

<style scoped>
.image-history {
  display: flex;
  flex-direction: column;
  gap: var(--space-xs);
  width: 100%;
  margin: 0;
  padding: 0;
  list-style: none;
}

.image-history__item {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-sm);
  padding: var(--space-xs) var(--space-sm);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-control);
}

.image-history__item--current {
  border-color: var(--color-accent);
}

.image-history__text {
  display: flex;
  flex: 1;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}

.image-history__path {
  font-weight: 600;
}

.image-history__actions {
  display: flex;
  flex: none;
  gap: var(--space-xs);
}
</style>
