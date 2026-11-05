<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { useHostErrorText } from '../composables/useHostErrorText'
import type { FilePreview, HostError } from '../services/host'
import { formatBytes } from '../utils/format'

const HEX_BYTES_PER_LINE = 16
/** 这些控制字符以外的（制表、换行、回车、换页）超过这个比例就不当作文本 */
const MAX_CONTROL_RATIO = 0.02

type TextEncoding = 'utf8' | 'utf16le' | 'utf16be' | 'gbk'
/** 一段字节怎么显示：读得通的文本，或十六进制 */
type Shown = { kind: 'text'; text: string; encoding: TextEncoding } | { kind: 'hex'; lines: string[] }

/**
 * 引擎给出的预览：图片是引擎解码、缩小后重新编码的 PNG；其他内容显示开头的几 KiB，
 * 读得通是文本（UTF-8、带字节序标记的 UTF-16、GBK）时显示文字，否则显示十六进制，可以切换。
 * NTFS 文件另列备用数据流。文件本身从不交给 WebView 渲染。
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

/** 用户选了看十六进制（换一个文件时回到默认） */
const isHexChosen = ref(false)

const thumbnailSource = computed(() =>
  props.preview?.image ? `data:image/png;base64,${props.preview.image.thumbnail}` : null,
)
const headBytes = computed(() => (props.preview ? decodeBase64(props.preview.head) : new Uint8Array()))
const headText = computed(() => decodeText(headBytes.value))
const shownHead = computed<Shown>(() =>
  headText.value && !isHexChosen.value ? headText.value : { kind: 'hex', lines: formatHex(headBytes.value) },
)
const streams = computed(() =>
  (props.preview?.streams ?? []).map((stream) => {
    const bytes = decodeBase64(stream.head)
    return {
      name: stream.name,
      length: stream.length,
      isEmpty: bytes.length === 0 && stream.length > 0,
      shown: decodeText(bytes) ?? ({ kind: 'hex', lines: formatHex(bytes) } as Shown),
    }
  }),
)

watch(
  () => props.preview,
  () => {
    isHexChosen.value = false
  },
)

/**
 * 按内容判断是不是文本：有字节序标记的按它解；否则先试 UTF-8，不通再试 GBK。开头只是文件的一部分，
 * 末尾被截断的半个字符不算错（流式解码）。解不通、有 0 字节或控制字符太多时为 null。
 */
function decodeText(bytes: Uint8Array): Shown | null {
  if (bytes.length === 0) {
    return null
  }
  const withBom: [number[], TextEncoding, string][] = [
    [[0xef, 0xbb, 0xbf], 'utf8', 'utf-8'],
    [[0xff, 0xfe], 'utf16le', 'utf-16le'],
    [[0xfe, 0xff], 'utf16be', 'utf-16be'],
  ]
  const marked = withBom.find(([bom]) => bom.every((byte, index) => bytes[index] === byte))
  const candidates: [TextEncoding, string][] = marked
    ? [[marked[1], marked[2]]]
    : [
        ['utf8', 'utf-8'],
        ['gbk', 'gbk'],
      ]
  for (const [encoding, label] of candidates) {
    let text: string
    try {
      text = new TextDecoder(label, { fatal: true }).decode(bytes, { stream: true })
    } catch {
      continue
    }
    if (looksLikeText(text)) {
      return { kind: 'text', text, encoding }
    }
  }
  return null
}

function looksLikeText(text: string): boolean {
  let controls = 0
  for (const char of text) {
    const code = char.charCodeAt(0)
    if (code === 0) {
      return false
    }
    if (code < 0x20 && code !== 0x09 && code !== 0x0a && code !== 0x0d && code !== 0x0c) {
      controls += 1
    }
  }
  return controls <= text.length * MAX_CONTROL_RATIO
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
  <div class="file-preview-view" aria-live="polite">
    <p v-if="isLoading" class="wizard__note">{{ t('preview.loading') }}</p>
    <p v-else-if="error" class="wizard__error">{{ formatHostError(error) }}</p>
    <template v-else-if="preview">
      <figure v-if="thumbnailSource && preview.image" class="file-preview-view__figure">
        <img
          class="file-preview-view__image"
          :src="thumbnailSource"
          :alt="t('preview.thumbnailAlt', { name })"
        />
        <figcaption class="wizard__note">
          {{ t('preview.imageSize', { width: preview.image.width, height: preview.image.height }) }}
        </figcaption>
      </figure>
      <p v-if="preview.imageError" class="wizard__note">
        {{ t('preview.imageError', { reason: preview.imageError }) }}
      </p>
      <template v-if="!preview.image">
        <div class="file-preview-view__bar">
          <p class="wizard__note">
            {{
              shownHead.kind === 'text'
                ? t('preview.textTitle', {
                    bytes: formatBytes(headBytes.length),
                    encoding: t(`preview.encoding.${shownHead.encoding}`),
                  })
                : t('preview.hexTitle', { bytes: formatBytes(headBytes.length) })
            }}
          </p>
          <button
            v-if="headText"
            class="file-preview-view__switch"
            type="button"
            @click="isHexChosen = !isHexChosen"
          >
            {{ isHexChosen ? t('preview.showText') : t('preview.showHex') }}
          </button>
        </div>
        <pre v-if="shownHead.kind === 'text'" class="file-preview-view__box file-preview-view__text">{{
          shownHead.text
        }}</pre>
        <pre v-else class="file-preview-view__box">{{ shownHead.lines.join('\n') }}</pre>
      </template>
      <section v-if="streams.length" class="file-preview-view__streams">
        <h4 class="wizard__note">{{ t('preview.streamsTitle', { count: streams.length }) }}</h4>
        <p class="wizard__note">{{ t('preview.streamsHint') }}</p>
        <div v-for="stream in streams" :key="stream.name" class="file-preview-view__stream">
          <p class="file-preview-view__stream-name">
            {{ t('preview.streamLine', { name: stream.name, size: formatBytes(stream.length) }) }}
          </p>
          <p v-if="stream.isEmpty" class="wizard__note">{{ t('preview.streamUnreadable') }}</p>
          <pre
            v-else-if="stream.shown.kind === 'text'"
            class="file-preview-view__box file-preview-view__text file-preview-view__box--small"
            >{{ stream.shown.text }}</pre
          >
          <pre v-else-if="stream.length > 0" class="file-preview-view__box file-preview-view__box--small">{{
            stream.shown.lines.join('\n')
          }}</pre>
        </div>
      </section>
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
  border-radius: var(--radius-control);
}

.file-preview-view__bar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-sm);
}

.file-preview-view__switch {
  flex: none;
  padding: 0;
  color: var(--color-accent);
  font-size: var(--font-size-small);
  background: none;
  border: none;
  cursor: pointer;
}

.file-preview-view__switch:hover {
  text-decoration: underline;
}

.file-preview-view__streams {
  display: flex;
  flex-direction: column;
  gap: var(--space-xs);
  margin-top: var(--space-sm);
}

.file-preview-view__stream {
  display: flex;
  flex-direction: column;
  gap: var(--space-xs);
}

.file-preview-view__stream-name {
  margin: 0;
  font-size: var(--font-size-small);
  overflow-wrap: anywhere;
}

.file-preview-view__text {
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}

.file-preview-view__box--small {
  max-height: 120px;
}

.file-preview-view__box {
  max-height: 280px;
  margin: 0;
  padding: var(--space-sm);
  overflow: auto;
  font-family: ui-monospace, 'Cascadia Mono', Consolas, monospace;
  font-size: var(--font-size-small);
  line-height: 1.5;
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-control);
  user-select: text;
}
</style>
