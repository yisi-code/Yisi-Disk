<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { useHostErrorText } from '../composables/useHostErrorText'
import {
  previewRecoveredFile,
  toHostError,
  type FilePreview,
  type FoundFile,
  type HostError,
} from '../services/host'
import { formatBytes } from '../utils/format'

/** 选中后等这么久再请引擎读取：用方向键快速移动时，不为途经的每个文件都起一个引擎 */
const PREVIEW_DELAY_MS = 150
const HEX_BYTES_PER_LINE = 16
/** 文件名里的扇区号按 512 字节计 */
const NAME_SECTOR_SIZE = 512

const props = defineProps<{
  jobId: string | null
  file: FoundFile | null
  typeName: string
  /** 打开、在文件夹中显示失败时由父组件给出 */
  actionError: HostError | null
}>()
const emit = defineEmits<{ open: [file: FoundFile]; reveal: [file: FoundFile] }>()

const { t } = useI18n()
const { formatHostError } = useHostErrorText()

const preview = ref<FilePreview | null>(null)
const previewError = ref<HostError | null>(null)
const isLoading = ref(false)

let pendingTimer: ReturnType<typeof setTimeout> | undefined
/** 每次选中换一个序号，旧请求晚到的答复丢弃 */
let requestSerial = 0

const thumbnailSource = computed(() =>
  preview.value?.image ? `data:image/png;base64,${preview.value.image.thumbnail}` : null,
)

const headBytes = computed(() => (preview.value ? decodeBase64(preview.value.head) : new Uint8Array()))
const hexLines = computed(() => formatHex(headBytes.value))

watch(
  () => [props.jobId, props.file?.name] as const,
  () => {
    clearTimeout(pendingTimer)
    requestSerial += 1
    preview.value = null
    previewError.value = null
    const { jobId, file } = props
    if (jobId === null || file === null) {
      isLoading.value = false
      return
    }
    isLoading.value = true
    const serial = requestSerial
    pendingTimer = setTimeout(() => void loadPreview(jobId, file.name, serial), PREVIEW_DELAY_MS)
  },
  { immediate: true },
)

onBeforeUnmount(() => {
  clearTimeout(pendingTimer)
  requestSerial += 1
})

async function loadPreview(jobId: string, name: string, serial: number) {
  try {
    const result = await previewRecoveredFile(jobId, name)
    if (serial === requestSerial) {
      preview.value = result
    }
  } catch (error) {
    if (serial === requestSerial) {
      previewError.value = toHostError(error)
    }
  } finally {
    if (serial === requestSerial) {
      isLoading.value = false
    }
  }
}

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
  <section class="recovered-preview" :aria-label="t('carve.preview.title')">
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
        <dt>{{ t('carve.preview.position') }}</dt>
        <dd>
          {{
            t('carve.preview.positionValue', {
              position: formatBytes(file.offset),
              sector: Math.floor(file.offset / NAME_SECTOR_SIZE),
            })
          }}
        </dd>
      </dl>

      <div class="recovered-preview__actions">
        <button
          class="recovered-preview__button"
          type="button"
          :disabled="jobId === null"
          @click="emit('open', file)"
        >
          {{ t('carve.preview.open') }}
        </button>
        <button
          class="recovered-preview__button recovered-preview__button--secondary"
          type="button"
          :disabled="jobId === null"
          @click="emit('reveal', file)"
        >
          {{ t('carve.preview.reveal') }}
        </button>
      </div>
      <p v-if="actionError" class="recovered-preview__error" role="alert">
        {{ formatHostError(actionError) }}
      </p>

      <div class="recovered-preview__body" aria-live="polite">
        <p v-if="isLoading" class="recovered-preview__note">{{ t('carve.preview.loading') }}</p>
        <p v-else-if="previewError" class="recovered-preview__error">
          {{ formatHostError(previewError) }}
        </p>
        <template v-else-if="preview">
          <figure v-if="thumbnailSource && preview.image" class="recovered-preview__figure">
            <img
              class="recovered-preview__image"
              :src="thumbnailSource"
              :alt="t('carve.preview.thumbnailAlt', { name: file.name })"
            />
            <figcaption class="recovered-preview__note">
              {{
                t('carve.preview.imageSize', {
                  width: preview.image.width,
                  height: preview.image.height,
                })
              }}
            </figcaption>
          </figure>
          <p v-if="preview.imageError" class="recovered-preview__note">
            {{ t('carve.preview.imageError', { reason: preview.imageError }) }}
          </p>
          <template v-if="!preview.image">
            <p class="recovered-preview__note">
              {{ t('carve.preview.hexTitle', { bytes: formatBytes(headBytes.length) }) }}
            </p>
            <pre class="recovered-preview__hex">{{ hexLines.join('\n') }}</pre>
          </template>
        </template>
      </div>
    </template>
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

.recovered-preview__body {
  display: flex;
  flex-direction: column;
  gap: var(--space-xs);
}

.recovered-preview__figure {
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: var(--space-xs);
  margin: 0;
}

.recovered-preview__image {
  display: block;
  max-width: 100%;
  max-height: 320px;
  border: 1px solid var(--color-border);
  border-radius: var(--space-xs);
}

.recovered-preview__hex {
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
