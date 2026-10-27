<script setup lang="ts">
import { computed, useId } from 'vue'
import { useI18n } from 'vue-i18n'
import FilePreviewView from './FilePreviewView.vue'
import { useDelayedPreview } from '../composables/useDelayedPreview'
import { useHostErrorText } from '../composables/useHostErrorText'
import {
  previewCarveRange,
  previewRecoveredFile,
  type FoundFile,
  type HostError,
} from '../services/host'
import { formatBytes } from '../utils/format'

/** 文件名里的扇区号按 512 字节计 */
const NAME_SECTOR_SIZE = 512

const props = defineProps<{
  jobId: string | null
  file: FoundFile | null
  typeName: string
  /** 打开、在文件夹中显示失败时由父组件给出 */
  actionError: HostError | null
  /** 只扫描、没有边扫边保存：预览从源上读，打开前要先保存一份副本 */
  scanOnly: boolean
  /** 文件已经存在用户的保存位置（边扫边保存的，或另存过），可以在文件夹中显示 */
  isSaved: boolean
  /** 不能打开、另存时（例如正在扫描或正在保存）由父组件给出 */
  canOpen: boolean
  canSaveAs: boolean
}>()
const emit = defineEmits<{
  open: [file: FoundFile]
  saveAs: [file: FoundFile]
  reveal: [file: FoundFile]
}>()

const { t, locale } = useI18n()
const { formatHostError } = useHostErrorText()
const titleId = useId()

const { preview, error: previewError, isLoading } = useDelayedPreview(
  () => (props.jobId !== null && props.file !== null ? { jobId: props.jobId, file: props.file } : null),
  (target) => `${target.jobId}/${target.file.name}`,
  (target) =>
    props.scanOnly
      ? previewCarveRange(target.jobId, target.file.offset, target.file.length)
      : previewRecoveredFile(target.jobId, target.file.name),
)

/** 起止都按字节偏移给出，结束位置是文件最后一个字节 */
const extent = computed(() => {
  const file = props.file
  if (file === null) {
    return null
  }
  return { start: locate(file.offset), end: locate(file.offset + Math.max(file.length, 1) - 1) }
})

function locate(offset: number) {
  return {
    position: formatBytes(offset),
    offset: offset.toLocaleString(locale.value),
    sector: Math.floor(offset / NAME_SECTOR_SIZE),
  }
}
</script>

<template>
  <section class="recovered-preview" :aria-labelledby="titleId">
    <h3 :id="titleId" class="recovered-preview__title">
      {{ t('carve.preview.title') }}
    </h3>
    <div class="recovered-preview__card">
      <p v-if="file === null" class="recovered-preview__placeholder">
        {{ t('carve.preview.placeholder') }}
      </p>
      <template v-else>
        <dl class="recovered-preview__details">
          <dt>{{ t('carve.preview.name') }}</dt>
          <dd class="recovered-preview__selectable">{{ file.name }}</dd>
          <dt>{{ t('carve.preview.format') }}</dt>
          <dd>{{ typeName }}</dd>
          <dt>{{ t('carve.preview.size') }}</dt>
          <dd>
            {{ t('carve.preview.sizeValue', { size: formatBytes(file.length), bytes: file.length }) }}
          </dd>
          <template v-if="extent">
            <dt>{{ t('carve.preview.start') }}</dt>
            <dd>{{ t('carve.preview.positionValue', extent.start) }}</dd>
            <dt>{{ t('carve.preview.end') }}</dt>
            <dd>{{ t('carve.preview.positionValue', extent.end) }}</dd>
          </template>
        </dl>

        <div class="recovered-preview__actions">
          <button
            class="recovered-preview__button"
            type="button"
            :disabled="jobId === null || !canOpen"
            @click="emit('open', file)"
          >
            {{ t('carve.preview.open') }}
          </button>
          <button
            class="recovered-preview__button recovered-preview__button--secondary"
            type="button"
            :disabled="jobId === null || !canSaveAs"
            @click="emit('saveAs', file)"
          >
            {{ t('carve.preview.saveAs') }}
          </button>
          <button
            class="recovered-preview__button recovered-preview__button--secondary"
            type="button"
            :disabled="jobId === null || !isSaved"
            @click="emit('reveal', file)"
          >
            {{ t('carve.preview.reveal') }}
          </button>
        </div>
        <p v-if="actionError" class="recovered-preview__error" role="alert">
          {{ formatHostError(actionError) }}
        </p>

        <FilePreviewView
          :preview="preview"
          :is-loading="isLoading"
          :error="previewError"
          :name="file.name"
        />
      </template>
    </div>
    <p v-if="file !== null && !isSaved" class="recovered-preview__note">
      {{ t('carve.preview.notSavedHint') }}
    </p>
  </section>
</template>

<style scoped>
.recovered-preview {
  display: flex;
  flex-direction: column;
  gap: var(--space-sm);
  width: 100%;
  min-width: 0;
}

.recovered-preview__title {
  align-self: center;
  margin: 0;
  font-size: var(--font-size-body);
  font-weight: 600;
}

/* 高度跟随所在布局（深度恢复里与左边的文件列表框等高），不按内容撑开；内容超出时在框里滚动 */
.recovered-preview__card {
  display: flex;
  flex-direction: column;
  gap: var(--space-sm);
  min-height: 240px;
  padding: var(--space-sm);
  overflow-y: auto;
  border: 1px solid var(--color-border);
  border-radius: var(--radius-card);
  contain: size;
}

.recovered-preview__placeholder,
.recovered-preview__note {
  margin: 0;
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
}

.recovered-preview__details {
  display: grid;
  grid-template-columns: max-content 1fr;
  gap: var(--space-xs) var(--space-md);
  margin: 0;
}

.recovered-preview__details dt {
  color: var(--color-text-muted);
}

.recovered-preview__details dd {
  margin: 0;
  font-variant-numeric: tabular-nums;
  overflow-wrap: anywhere;
}

.recovered-preview__selectable {
  user-select: text;
}

.recovered-preview__actions {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-sm);
}

.recovered-preview__button {
  padding: var(--space-xs) var(--space-md);
  color: var(--color-on-accent);
  background: var(--color-accent);
  border: 1px solid var(--color-accent);
  border-radius: var(--space-xs);
  font: inherit;
  cursor: pointer;
}

.recovered-preview__button--secondary {
  color: var(--color-text);
  background: var(--color-surface);
  border-color: var(--color-border);
}

.recovered-preview__button:disabled {
  opacity: 0.5;
  cursor: default;
}

.recovered-preview__error {
  margin: 0;
  color: var(--color-danger);
}
</style>
