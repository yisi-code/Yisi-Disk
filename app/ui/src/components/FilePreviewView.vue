<script setup lang="ts">
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import { useHostErrorText } from '../composables/useHostErrorText'
import type { FilePreview, HostError } from '../services/host'
import { formatBytes } from '../utils/format'

const HEX_BYTES_PER_LINE = 16

/**
 * 引擎给出的预览：图片是引擎解码、缩小后重新编码的 PNG，其他内容只显示开头的十六进制，
 * 文件本身从不交给 WebView 渲染。
 */
const props = defineProps<{
  preview: FilePreview | null
  isLoading: boolean
  error: HostError | null
  /** 文件名，用作缩略图的替代文本 */
  name: string
}>()

const { t } = useI18n()
const { formatHostError } = useHostErrorText()

const thumbnailSource = computed(() =>
  props.preview?.image ? `data:image/png;base64,${props.preview.image.thumbnail}` : null,
)
const headBytes = computed(() => (props.preview ? decodeBase64(props.preview.head) : new Uint8Array()))
const hexLines = computed(() => formatHex(headBytes.value))

function decodeBase64(text: string): Uint8Array {
  const binary = atob(text)
  const bytes = new Uint8Array(binary.length)
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index)
  }
  return bytes
}

/** 每行：偏移、16 个字节的十六进制、可打印 ASCII（其他字节显示为点） */
function formatHex(bytes: Uint8Array): string[] {
  const lines: string[] = []
  for (let start = 0; start < bytes.length; start += HEX_BYTES_PER_LINE) {
    const line = bytes.subarray(start, start + HEX_BYTES_PER_LINE)
    const hex = Array.from(line, (byte) => byte.toString(16).padStart(2, '0')).join(' ')
    const ascii = Array.from(line, (byte) =>
      byte >= 0x20 && byte < 0x7f ? String.fromCharCode(byte) : '.',
    ).join('')
    lines.push(
      `${start.toString(16).padStart(8, '0')}  ${hex.padEnd(HEX_BYTES_PER_LINE * 3 - 1)}  ${ascii}`,
    )
  }
  return lines
}
</script>

<template>
  <div class="file-preview-view" aria-live="polite">
    <p v-if="isLoading" class="file-preview-view__note">{{ t('preview.loading') }}</p>
    <p v-else-if="error" class="file-preview-view__error">{{ formatHostError(error) }}</p>
    <template v-else-if="preview">
      <figure v-if="thumbnailSource && preview.image" class="file-preview-view__figure">
        <img
          class="file-preview-view__image"
          :src="thumbnailSource"
          :alt="t('preview.thumbnailAlt', { name })"
        />
        <figcaption class="file-preview-view__note">
          {{ t('preview.imageSize', { width: preview.image.width, height: preview.image.height }) }}
        </figcaption>
      </figure>
      <p v-if="preview.imageError" class="file-preview-view__note">
        {{ t('preview.imageError', { reason: preview.imageError }) }}
      </p>
      <template v-if="!preview.image">
        <p class="file-preview-view__note">
          {{ t('preview.hexTitle', { bytes: formatBytes(headBytes.length) }) }}
        </p>
        <pre class="file-preview-view__hex">{{ hexLines.join('\n') }}</pre>
      </template>
    </template>
  </div>
</template>

<style scoped>
.file-preview-view {
  display: flex;
  flex-direction: column;
  gap: var(--space-xs);
  width: 100%;
  min-width: 0;
}

.file-preview-view__note {
  margin: 0;
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
}

.file-preview-view__error {
  margin: 0;
  color: var(--color-danger);
}

.file-preview-view__figure {
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: var(--space-xs);
  margin: 0;
}

.file-preview-view__image {
  display: block;
  max-width: 100%;
  max-height: 320px;
  border: 1px solid var(--color-border);
  border-radius: var(--space-xs);
}

.file-preview-view__hex {
  max-height: 280px;
  margin: 0;
  padding: var(--space-sm);
  overflow: auto;
  font-family: ui-monospace, 'Cascadia Mono', Consolas, monospace;
  font-size: var(--font-size-small);
  line-height: 1.5;
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: var(--space-xs);
  user-select: text;
}
</style>
