<script setup lang="ts">
import { computed, useId } from 'vue'
import { useI18n } from 'vue-i18n'
import FilePreviewView from './FilePreviewView.vue'
import { useDelayedPreview } from '../composables/useDelayedPreview'
import { useHostErrorText } from '../composables/useHostErrorText'
import { usePositionText } from '../composables/usePositionText'
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
const { bytePosition } = usePositionText()
const { formatHostError } = useHostErrorText()
const titleId = useId()

/** 由两段拼成的文件（两段重组）：「由两段拼成：第一段 A–B，第二段 C–D」，A–D 是源上的字节偏移，含两端 */
const pieces = computed(() => {
  const fragments = props.file?.fragments
  if (fragments === undefined || fragments.length < 2) {
    return null
  }
  const [first, second] = fragments.map((fragment) =>
    t('carve.preview.fragment', {
      start: fragment.offset.toLocaleString(locale.value),
      end: (fragment.offset + Math.max(fragment.length, 1) - 1).toLocaleString(locale.value),
      size: formatBytes(fragment.length),
    }),
  )
  return t('carve.preview.fragmentsValue', { first, second })
})

const { preview, error: previewError, isLoading } = useDelayedPreview(
  () => (props.jobId !== null && props.file !== null ? { jobId: props.jobId, file: props.file } : null),
  (target) => `${target.jobId}/${target.file.name}`,
  (target) =>
    props.scanOnly
      ? previewCarveRange(target.jobId, target.file.offset, target.file.length, target.file.fragments)
      : previewRecoveredFile(target.jobId, target.file.name),
)

/** 起止都按字节偏移给出，结束位置是文件最后一个字节；由两段拼成的文件改为分段给出 */
const extent = computed(() => {
  const file = props.file
  if (file === null || pieces.value !== null) {
    return null
  }
  return {
    start: bytePosition(file.offset, NAME_SECTOR_SIZE),
    end: bytePosition(file.offset + Math.max(file.length, 1) - 1, NAME_SECTOR_SIZE),
  }
})
</script>

<template>
  <section class="detail-card" :aria-labelledby="titleId">
    <h3 :id="titleId" class="detail-card__title">
      {{ t('carve.preview.title') }}
    </h3>
    <div class="detail-card__card">
      <p v-if="file === null" class="recovered-preview__placeholder">
        {{ t('carve.preview.placeholder') }}
      </p>
      <template v-else>
        <dl class="detail-card__list">
          <dt>{{ t('carve.preview.name') }}</dt>
          <dd class="recovered-preview__selectable">{{ file.name }}</dd>
          <dt>{{ t('carve.preview.format') }}</dt>
          <dd>{{ typeName }}</dd>
          <dt>{{ t('carve.preview.size') }}</dt>
          <dd>
            {{ t('common.sizeValue', { size: formatBytes(file.length), bytes: file.length }) }}
          </dd>
          <template v-if="extent">
            <dt>{{ t('carve.preview.start') }}</dt>
            <dd>{{ extent.start }}</dd>
            <dt>{{ t('carve.preview.end') }}</dt>
            <dd>{{ extent.end }}</dd>
          </template>
          <template v-if="pieces !== null">
            <dt>{{ t('carve.preview.fragments') }}</dt>
            <dd class="recovered-preview__selectable">{{ pieces }}</dd>
          </template>
          <template v-if="file.incomplete">
            <dt>{{ t('carve.preview.state') }}</dt>
            <dd>
              {{ t('carve.preview.incomplete', { reason: t(`carve.incomplete.${file.incomplete}`) }) }}
              <span class="recovered-preview__note">{{ t('carve.preview.incompleteHint') }}</span>
            </dd>
          </template>
        </dl>

        <div class="detail-card__actions">
          <button
            class="wizard__button"
            type="button"
            :disabled="jobId === null || !canOpen"
            @click="emit('open', file)"
          >
            {{ t('carve.preview.open') }}
          </button>
          <button
            class="wizard__button wizard__button--secondary"
            type="button"
            :disabled="jobId === null || !canSaveAs"
            @click="emit('saveAs', file)"
          >
            {{ t('carve.preview.saveAs') }}
          </button>
          <button
            class="wizard__button wizard__button--secondary"
            type="button"
            :disabled="jobId === null || !isSaved"
            @click="emit('reveal', file)"
          >
            {{ t('common.reveal') }}
          </button>
        </div>
        <p v-if="actionError" class="wizard__error" role="alert">
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
    <p v-if="file !== null && !isSaved" class="wizard__note">
      {{ t('carve.preview.notSavedHint') }}
    </p>
  </section>
</template>

<style scoped>

.recovered-preview__placeholder {
  margin: 0;
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
}

.recovered-preview__selectable {
  user-select: text;
}

.recovered-preview__note {
  display: block;
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
}
</style>
