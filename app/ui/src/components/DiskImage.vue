<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import SourcePicker from './SourcePicker.vue'
import UnreadableRanges from './UnreadableRanges.vue'
import { useDiskImage } from '../composables/useDiskImage'
import { useHostErrorText } from '../composables/useHostErrorText'
import { useHostDialog } from '../composables/useHostDialog'
import type { JobTarget } from '../services/host'
import { formatBytes, formatSeconds } from '../utils/format'

/** 报告里最多列出的读不出区段数；其余只计入总数 */
const MAX_LISTED_RANGES = 20

const emit = defineEmits<{ exit: [] }>()

const { t } = useI18n()
const { formatHostError } = useHostErrorText()
const {
  status,
  imaging,
  summary,
  lastRequest,
  failure,
  isUnresponsive,
  isBusy,
  start,
  cancel,
} = useDiskImage()

const titleElement = ref<HTMLHeadingElement | null>(null)
// 做镜像通常是为了出问题的物理盘，默认选它
const sourceKind = ref<JobTarget['kind']>('disk')
const sourcePath = ref('')
const selectedDisk = ref<number | null>(null)
const imagePath = ref('')
const { dialogError, pickFolder } = useHostDialog()

const imageTarget = computed<JobTarget | null>(() => {
  if (sourceKind.value === 'image') {
    const path = sourcePath.value.trim()
    return path === '' ? null : { kind: 'image', path }
  }
  return selectedDisk.value === null ? null : { kind: 'disk', diskNumber: selectedDisk.value }
})
const trimmedImagePath = computed(() => imagePath.value.trim())
const canStart = computed(
  () => imageTarget.value !== null && trimmedImagePath.value !== '' && !isBusy.value,
)

/** 新建时这个位置已经有镜像：可以按它的坏区图续做 */
const canResumeExisting = computed(
  () => status.value === 'failed' && failure.value?.code === 'imageExists' && lastRequest.value !== null,
)
/** 源设备读不到了：结果里带着停下的位置，检查连接后可以续做 */
const sourceLost = computed(
  () => status.value === 'completed' && summary.value !== null && summary.value.sourceLostAt !== null,
)
const canContinue = computed(
  () => (status.value === 'cancelled' || sourceLost.value) && lastRequest.value !== null,
)
const finishedSummary = computed(() =>
  status.value === 'completed' && !sourceLost.value ? summary.value : null,
)
const listedRanges = computed(
  () => finishedSummary.value?.badRanges.slice(0, MAX_LISTED_RANGES) ?? [],
)

/** 已经有结论的字节数（读出或确认读不出），进度条按它走 */
const settledBytes = computed(() => {
  const current = imaging.value
  if (current === null) {
    return 0
  }
  return current.size - current.untriedBytes - current.trimBytes - current.scrapeBytes
})

const remainingText = computed(() => {
  const current = imaging.value
  // 只在复制这一轮估计：之后按扇区读坏区，速度说明不了什么
  if (current === null || current.round !== 'copy' || !current.bytesPerSecond) {
    return null
  }
  return formatRemaining(current.untriedBytes / current.bytesPerSecond)
})

const statusText = computed(() => {
  switch (status.value) {
    case 'idle':
      return ''
    case 'starting':
      return lastRequest.value?.target.kind === 'disk'
        ? t('common.startingDisk')
        : t('common.startingEngine')
    case 'running':
      return imaging.value === null
        ? t('diskImage.status.preparing')
        : t('diskImage.status.running', {
            round: t(`diskImage.round.${imaging.value.round}`),
            percent:
              imaging.value.size > 0
                ? Math.floor((settledBytes.value / imaging.value.size) * 100)
                : 100,
          })
    case 'cancelling':
      return t('diskImage.status.stopping')
    case 'completed':
      return sourceLost.value ? t('diskImage.status.sourceLost') : t('diskImage.status.completed')
    case 'failed':
      return t('diskImage.status.failed')
    case 'cancelled':
      return t('diskImage.status.stopped')
  }
  return ''
})

onMounted(() => {
  // 从导航页切过来，焦点放到标题上，读屏软件从这里开始读
  titleElement.value?.focus()
})

function handleExit() {
  if (!isBusy.value) {
    emit('exit')
  }
}

function handleSubmit() {
  if (canStart.value && imageTarget.value !== null) {
    void start({ target: imageTarget.value, imagePath: trimmedImagePath.value }, false)
  }
}

function handleCancel() {
  void cancel()
}

/** 续做最近一次发起的镜像：新建时撞上了已有的镜像，或者停下、源设备读不到之后 */
function handleResume() {
  if (lastRequest.value !== null) {
    void start(lastRequest.value, true)
  }
}

/** 系统的选择文件夹对话框，选好后在后面接上默认的文件名；用户取消时什么也不改 */
async function browseImageFolder() {
  const folder = await pickFolder(t('diskImage.output.pickTitle'))
  if (folder !== null) {
    imagePath.value = joinPath(folder, defaultImageName())
  }
}

/** 物理磁盘取编号，镜像文件取原文件名：同一个源每次给出同一个名字，续做时容易对上 */
function defaultImageName(): string {
  if (sourceKind.value === 'disk') {
    return selectedDisk.value === null ? 'disk.img' : `disk${selectedDisk.value}.img`
  }
  const fileName = sourcePath.value.trim().split(/[\\/]/).pop() ?? ''
  const stem = fileName.replace(/\.[^.]*$/, '')
  return stem === '' ? 'disk-image.img' : `${stem}-image.img`
}

function joinPath(folder: string, name: string): string {
  return /[\\/]$/.test(folder) ? `${folder}${name}` : `${folder}\\${name}`
}

function formatRemaining(seconds: number): string {
  if (seconds < 60) {
    return t('diskImage.run.remainingUnderMinute')
  }
  const minutes = Math.round(seconds / 60)
  return minutes < 60
    ? t('diskImage.run.remainingMinutes', { minutes })
    : t('diskImage.run.remainingHours', { hours: Math.floor(minutes / 60), minutes: minutes % 60 })
}

</script>

<template>
  <section class="wizard" aria-labelledby="disk-image-title">
    <header class="wizard__header">
      <button
        class="wizard__button wizard__button--secondary"
        type="button"
        :disabled="isBusy"
        @click="handleExit"
      >
        {{ t('nav.back') }}
      </button>
      <h2 id="disk-image-title" ref="titleElement" class="wizard__title" tabindex="-1">
        {{ t('features.image.name') }}
      </h2>
    </header>

    <div class="tool__body">
      <p class="tool__intro">{{ t('diskImage.intro') }}</p>

      <details class="tool__guide" open>
        <summary class="tool__guide-summary">{{ t('diskImage.guide.title') }}</summary>
        <ul class="tool__guide-list">
          <li>{{ t('diskImage.guide.copy') }}</li>
          <li>{{ t('diskImage.guide.trimScrape') }}</li>
          <li>{{ t('diskImage.guide.map') }}</li>
          <li>{{ t('diskImage.guide.resume') }}</li>
          <li>{{ t('diskImage.guide.space') }}</li>
        </ul>
      </details>

      <form class="wizard__form" @submit.prevent="handleSubmit">
        <fieldset class="disk-image__section" :disabled="isBusy">
          <legend class="disk-image__section-title">{{ t('diskImage.source.title') }}</legend>
          <p class="wizard__hint">{{ t('diskImage.source.hint') }}</p>
          <SourcePicker
            v-model:kind="sourceKind"
            v-model:image-path="sourcePath"
            v-model:disk="selectedDisk"
            :disk-hint="t('diskImage.diskHint')"
            :image-hint="t('diskImage.source.imageHint')"
          />
        </fieldset>

        <fieldset class="disk-image__section" :disabled="isBusy">
          <legend class="disk-image__section-title">{{ t('diskImage.output.title') }}</legend>
          <div class="wizard__path">
            <label class="wizard__label" for="disk-image-path">{{ t('diskImage.output.label') }}</label>
            <input
              id="disk-image-path"
              v-model="imagePath"
              class="wizard__input"
              type="text"
              spellcheck="false"
              autocomplete="off"
              aria-describedby="disk-image-path-hint"
              :placeholder="t('diskImage.output.placeholder')"
            />
            <button
              class="wizard__button wizard__button--secondary"
              type="button"
              @click="browseImageFolder"
            >
              {{ t('common.browseFolder') }}
            </button>
          </div>
          <p id="disk-image-path-hint" class="wizard__hint">{{ t('diskImage.output.hint') }}</p>
          <p v-if="sourceKind === 'disk'" class="wizard__hint wizard__hint--warning">
            {{ t('diskImage.output.diskHint') }}
          </p>
          <p v-if="dialogError" class="wizard__error" role="alert">
            {{ formatHostError(dialogError) }}
          </p>
        </fieldset>

        <div class="wizard__nav">
          <span class="wizard__nav-gap"></span>
          <button v-if="!isBusy" class="wizard__button" type="submit" :disabled="!canStart">
            {{ t('diskImage.start') }}
          </button>
          <button
            v-else
            class="wizard__button wizard__button--secondary"
            type="button"
            :disabled="status !== 'running'"
            @click="handleCancel"
          >
            {{ status === 'cancelling' ? t('diskImage.stopping') : t('diskImage.stop') }}
          </button>
        </div>
      </form>

      <div v-if="status !== 'idle'" class="wizard__job" aria-live="polite">
        <progress
          v-if="imaging"
          class="wizard__progress"
          :value="settledBytes"
          :max="imaging.size || 1"
        ></progress>
        <progress v-else-if="isBusy" class="wizard__progress"></progress>
        <p class="wizard__status">{{ statusText }}</p>

        <!-- 做完后由下方的报告给出结果，这里不再重复 -->
        <dl v-if="imaging && !finishedSummary" class="wizard__details">
          <dt>{{ t('diskImage.run.round') }}</dt>
          <dd>{{ t(`diskImage.round.${imaging.round}`) }}</dd>
          <dt>{{ t('diskImage.run.read') }}</dt>
          <dd>{{ formatBytes(imaging.readBytes) }} / {{ formatBytes(imaging.size) }}</dd>
          <dt>{{ t('diskImage.run.bad') }}</dt>
          <dd>{{ formatBytes(imaging.badBytes) }}</dd>
          <dt>{{ t('diskImage.run.pending') }}</dt>
          <dd>
            {{
              t('diskImage.run.pendingValue', {
                untried: formatBytes(imaging.untriedBytes),
                trim: formatBytes(imaging.trimBytes),
                scrape: formatBytes(imaging.scrapeBytes),
              })
            }}
          </dd>
          <template v-if="isBusy">
            <dt>{{ t('diskImage.run.rate') }}</dt>
            <dd>
              {{
                imaging.bytesPerSecond === null
                  ? t('diskImage.run.rateUnknown')
                  : t('diskImage.run.rateValue', { rate: formatBytes(imaging.bytesPerSecond) })
              }}
            </dd>
            <template v-if="remainingText">
              <dt>{{ t('diskImage.run.remaining') }}</dt>
              <dd>{{ remainingText }}</dd>
            </template>
          </template>
        </dl>

        <p v-if="isUnresponsive" class="wizard__error" role="alert">
          {{ t('diskImage.unresponsive') }}
        </p>
        <p v-if="failure" class="wizard__error" role="alert">{{ formatHostError(failure) }}</p>

        <div v-if="canResumeExisting" class="disk-image__resume">
          <p class="wizard__note">{{ t('diskImage.exists') }}</p>
          <button class="wizard__button" type="button" @click="handleResume">
            {{ t('diskImage.resumeExisting') }}
          </button>
        </div>
        <div v-if="canContinue && lastRequest" class="disk-image__resume">
          <p class="wizard__note">
            {{ t('diskImage.continueHint', { path: lastRequest.imagePath }) }}
          </p>
          <button class="wizard__button" type="button" @click="handleResume">
            {{ t('diskImage.continue') }}
          </button>
        </div>
      </div>

      <section
        v-if="finishedSummary"
        class="tool__report"
        aria-labelledby="disk-image-result-title"
      >
        <h3 id="disk-image-result-title" class="tool__report-title">
          {{
            finishedSummary.badBytes === 0
              ? t('diskImage.result.clean')
              : t('diskImage.result.found', { bytes: formatBytes(finishedSummary.badBytes) })
          }}
        </h3>

        <dl class="wizard__details">
          <dt>{{ t('diskImage.result.read') }}</dt>
          <dd>
            {{ formatBytes(finishedSummary.readBytes) }} / {{ formatBytes(finishedSummary.size) }}
          </dd>
          <dt>{{ t('diskImage.result.bad') }}</dt>
          <dd>
            {{
              t('diskImage.result.badValue', {
                count: finishedSummary.badRanges.length,
                bytes: formatBytes(finishedSummary.badBytes),
              }, finishedSummary.badRanges.length)
            }}
          </dd>
          <dt>{{ t('diskImage.result.elapsed') }}</dt>
          <dd>
            {{
              t('common.seconds', {
                seconds: formatSeconds(finishedSummary.elapsedMs),
              })
            }}
          </dd>
          <dt>{{ t('diskImage.result.imagePath') }}</dt>
          <dd class="wizard__path-text">{{ finishedSummary.imagePath }}</dd>
          <dt>{{ t('diskImage.result.mapPath') }}</dt>
          <dd class="wizard__path-text">{{ finishedSummary.mapPath }}</dd>
        </dl>

        <UnreadableRanges v-if="listedRanges.length > 0" :ranges="listedRanges" />
        <p v-if="finishedSummary.badRanges.length > MAX_LISTED_RANGES" class="wizard__note">
          {{
            t('diskImage.result.moreRanges', {
              shown: MAX_LISTED_RANGES,
              count: finishedSummary.badRanges.length,
            })
          }}
        </p>
        <p v-if="finishedSummary.badRangesTruncated" class="wizard__note">
          {{ t('diskImage.result.truncated', { count: finishedSummary.badRanges.length }) }}
        </p>

        <p class="tool__next">
          {{ t('diskImage.result.next', { path: finishedSummary.imagePath }) }}
          <template v-if="finishedSummary.badBytes > 0">{{ t('diskImage.result.nextBad') }}</template>
        </p>
      </section>
    </div>
  </section>
</template>

<style scoped>
.disk-image__section {
  display: flex;
  flex-direction: column;
  gap: var(--space-sm);
  min-width: 0;
  margin: 0;
  padding: var(--space-sm) var(--space-md) var(--space-md);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-card);
}

.disk-image__section-title {
  padding: 0 var(--space-xs);
  font-weight: 600;
}

.disk-image__resume {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-sm);
}
</style>
