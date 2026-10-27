<script setup lang="ts">
import { computed, useId } from 'vue'
import { useI18n } from 'vue-i18n'
import FilePreviewView from './FilePreviewView.vue'
import type { TreeItem } from './FsBrowser.vue'
import { useDelayedPreview } from '../composables/useDelayedPreview'
import { useHostErrorText } from '../composables/useHostErrorText'
import { previewFsFile, type HostError } from '../services/host'
import { formatBytes } from '../utils/format'

const props = defineProps<{
  jobId: string
  partition: number
  item: TreeItem | null
  /** 源的逻辑扇区大小，位置同时给出扇区号 */
  sectorSize: number
  /** 分区记录的是 UTC 时间（NTFS），时间后面标上 */
  timesUtc: boolean
  /** 复制任务进行中时不能打开（打开要先复制出来） */
  canOpen: boolean
  /** 打开失败、或打开前的复制失败时由父组件给出 */
  actionError: HostError | null
  /** 可执行类文件没有打开、改为在文件夹中显示时的说明 */
  actionNote: string | null
}>()
const emit = defineEmits<{ open: [item: TreeItem] }>()

const { t, locale } = useI18n()
const titleId = useId()
const { formatHostError } = useHostErrorText()

// 目录没有内容可预览；存放方式不支持的文件读不出原内容，也不预览
const { preview, error: previewError, isLoading } = useDelayedPreview(
  () =>
    props.item !== null && !props.item.entry.isDir && props.item.entry.unsupported === null
      ? props.item
      : null,
  (item) => `${props.jobId}/${props.partition}/${item.id}`,
  (item) => previewFsFile(props.jobId, props.partition, item.entry.file),
)

const stateText = computed(() => {
  const entry = props.item?.entry
  if (entry === undefined) {
    return ''
  }
  if (entry.virtualDir !== null) {
    return t(`partitionFiles.stateDetail.${entry.virtualDir}`)
  }
  return t(`partitionFiles.stateDetail.${entry.deleted ? entry.state : 'live'}`)
})

function formatTime(time: string): string {
  return props.timesUtc ? t('partitionFiles.details.utcTime', { time }) : time
}

const position = computed(() => {
  const value = props.item?.entry.position
  if (value === null || value === undefined) {
    return null
  }
  return t('common.positionValue', {
    position: formatBytes(value),
    offset: value.toLocaleString(locale.value),
    sector: Math.floor(value / props.sectorSize),
  })
})
</script>

<template>
  <section class="detail-card" :aria-labelledby="titleId">
    <h3 :id="titleId" class="detail-card__title">{{ t('partitionFiles.details.title') }}</h3>
    <div class="detail-card__card">
      <p v-if="item === null" class="fs-entry-details__note">{{ t('partitionFiles.details.placeholder') }}</p>
      <template v-else>
        <dl class="detail-card__list">
          <dt>{{ t('partitionFiles.details.name') }}</dt>
          <dd class="fs-entry-details__selectable">
            {{ item.entry.name }}
            <span v-if="item.entry.nameUncertain" class="fs-entry-details__note">
              {{ t('partitionFiles.browser.nameUncertain') }}
            </span>
          </dd>
          <template v-if="item.entry.shortName && item.entry.shortName !== item.entry.name">
            <dt>{{ t('partitionFiles.details.shortName') }}</dt>
            <dd>{{ item.entry.shortName }}</dd>
          </template>
          <dt>{{ t('partitionFiles.details.path') }}</dt>
          <dd class="fs-entry-details__selectable">/{{ item.path.join('/') }}</dd>
          <dt>{{ t('partitionFiles.details.state') }}</dt>
          <dd>{{ stateText }}</dd>
          <template v-if="item.entry.unsupported">
            <dt>{{ t('partitionFiles.details.storage') }}</dt>
            <dd>{{ t(`partitionFiles.unsupportedDetail.${item.entry.unsupported}`) }}</dd>
          </template>
          <template v-if="!item.entry.isDir">
            <dt>{{ t('partitionFiles.details.size') }}</dt>
            <dd>
              {{
                t('common.sizeValue', {
                  size: formatBytes(item.entry.size),
                  bytes: item.entry.size.toLocaleString(locale),
                })
              }}
            </dd>
          </template>
          <template v-if="item.entry.modified">
            <dt>{{ t('partitionFiles.details.modified') }}</dt>
            <dd>{{ formatTime(item.entry.modified) }}</dd>
          </template>
          <template v-if="item.entry.created">
            <dt>{{ t('partitionFiles.details.created') }}</dt>
            <dd>{{ formatTime(item.entry.created) }}</dd>
          </template>
          <template v-if="position">
            <dt>{{ t('partitionFiles.details.position') }}</dt>
            <dd>{{ position }}</dd>
          </template>
          <template v-if="item.entry.record !== null">
            <dt>{{ t('partitionFiles.details.record') }}</dt>
            <dd>{{ item.entry.record.toLocaleString(locale) }}</dd>
          </template>
        </dl>

        <template v-if="!item.entry.isDir">
          <div class="detail-card__actions">
            <button
              class="wizard__button"
              type="button"
              :disabled="!canOpen || item.entry.unsupported !== null"
              @click="emit('open', item)"
            >
              {{ t('partitionFiles.details.open') }}
            </button>
          </div>
          <p class="fs-entry-details__note">{{ t('partitionFiles.details.openHint') }}</p>
          <p v-if="actionNote" class="fs-entry-details__note">{{ actionNote }}</p>
          <p v-if="actionError" class="fs-entry-details__error" role="alert">
            {{ formatHostError(actionError) }}
          </p>
          <FilePreviewView
            v-if="item.entry.unsupported === null"
            :preview="preview"
            :is-loading="isLoading"
            :error="previewError"
            :name="item.entry.name"
          />
        </template>
      </template>
    </div>
    <slot name="footer"></slot>
  </section>
</template>

<style scoped>

.fs-entry-details__note {
  margin: 0;
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
}

.fs-entry-details__error {
  margin: 0;
  color: var(--color-danger);
}

.fs-entry-details__selectable {
  user-select: text;
}

</style>
