<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import SourcePicker from './SourcePicker.vue'
import UnreadableRanges from './UnreadableRanges.vue'
import JobControls from './JobControls.vue'
import { useHostErrorText } from '../composables/useHostErrorText'
import { useReadScan } from '../composables/useReadScan'
import type { JobTarget } from '../services/host'
import { formatBytes, formatSeconds } from '../utils/format'

/** 报告里最多列出的读不出区段数；其余只计入总数 */
const MAX_LISTED_RANGES = 20

const emit = defineEmits<{ exit: [] }>()

const { t } = useI18n()
const { formatHostError } = useHostErrorText()
const {
  status,
  targetKind,
  done,
  total,
  percent,
  bytesPerSecond,
  report,
  failure,
  isUnresponsive,
  isBusy,
  start,
  cancel,
} = useReadScan()

const titleElement = ref<HTMLHeadingElement | null>(null)
const sourceKind = ref<JobTarget['kind']>('image')
const imagePath = ref('')
const selectedDisk = ref<number | null>(null)

const scanTarget = computed<JobTarget | null>(() => {
  if (sourceKind.value === 'image') {
    const path = imagePath.value.trim()
    return path === '' ? null : { kind: 'image', path }
  }
  return selectedDisk.value === null ? null : { kind: 'disk', diskNumber: selectedDisk.value }
})

const canStart = computed(() => scanTarget.value !== null && !isBusy.value)

const completedReport = computed(() => (status.value === 'completed' ? report.value : null))
const listedRanges = computed(
  () => completedReport.value?.unreadable.slice(0, MAX_LISTED_RANGES) ?? [],
)

const statusText = computed(() => {
  switch (status.value) {
    case 'idle':
      return ''
    case 'starting':
      return targetKind.value === 'disk'
        ? t('common.startingDisk')
        : t('common.startingEngine')
    case 'running':
    case 'cancelling':
      return t('badSectors.status.running', {
        done: formatBytes(done.value),
        total: formatBytes(total.value),
        percent: percent.value,
        rate: bytesPerSecond.value === null ? '-' : formatBytes(bytesPerSecond.value),
      })
    case 'completed':
      return t('badSectors.status.completed')
    case 'failed':
      return t('badSectors.status.failed')
    case 'cancelled':
      return t('common.cancelled')
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
  if (canStart.value && scanTarget.value !== null) {
    void start(scanTarget.value)
  }
}

function handleCancel() {
  void cancel()
}

</script>

<template>
  <section class="wizard" aria-labelledby="bad-sectors-title">
    <header class="wizard__header">
      <button
        class="wizard__button wizard__button--secondary"
        type="button"
        :disabled="isBusy"
        @click="handleExit"
      >
        {{ t('nav.back') }}
      </button>
      <h2 id="bad-sectors-title" ref="titleElement" class="wizard__title" tabindex="-1">
        {{ t('features.badSectors.name') }}
      </h2>
    </header>

    <div class="tool__body">
      <p class="tool__intro">{{ t('badSectors.intro') }}</p>

      <details class="tool__guide" open>
        <summary class="tool__guide-summary">{{ t('badSectors.guide.title') }}</summary>
        <ul class="tool__guide-list">
          <li>{{ t('badSectors.guide.clean') }}</li>
          <li>{{ t('badSectors.guide.unreadable') }}</li>
          <li>{{ t('badSectors.guide.slow') }}</li>
          <li>{{ t('badSectors.guide.limits') }}</li>
        </ul>
      </details>

      <form class="wizard__form" @submit.prevent="handleSubmit">
        <fieldset class="bad-sectors__source" :disabled="isBusy">
          <SourcePicker
            v-model:kind="sourceKind"
            v-model:image-path="imagePath"
            v-model:disk="selectedDisk"
            :disk-hint="t('badSectors.diskHint')"
          />
        </fieldset>

        <div class="wizard__nav">
          <span class="wizard__nav-gap"></span>
          <button v-if="!isBusy" class="wizard__button" type="submit" :disabled="!canStart">
            {{ t('badSectors.start') }}
          </button>
          <JobControls v-else :status="status" @cancel="handleCancel" />
        </div>
      </form>

      <div v-if="status !== 'idle'" class="wizard__job" aria-live="polite">
        <progress class="wizard__progress" :value="done" :max="total || 1"></progress>
        <p class="wizard__status">{{ statusText }}</p>
        <p v-if="isUnresponsive" class="wizard__error" role="alert">
          {{ t('common.unresponsive') }}
        </p>
        <p v-if="failure" class="wizard__error" role="alert">{{ formatHostError(failure) }}</p>
      </div>

      <section
        v-if="completedReport"
        class="tool__report"
        aria-labelledby="bad-sectors-result-title"
      >
        <h3 id="bad-sectors-result-title" class="tool__report-title">
          {{
            completedReport.unreadableBytes === 0
              ? t('badSectors.result.clean')
              : t('badSectors.result.found', { bytes: formatBytes(completedReport.unreadableBytes) })
          }}
        </h3>

        <dl class="wizard__details">
          <dt>{{ t('badSectors.result.bytesRead') }}</dt>
          <dd>
            {{ formatBytes(completedReport.bytesRead) }} / {{ formatBytes(completedReport.size) }}
          </dd>
          <dt>{{ t('badSectors.result.unreadable') }}</dt>
          <dd>
            {{
              t('badSectors.result.unreadableValue', {
                count: completedReport.unreadable.length,
                bytes: formatBytes(completedReport.unreadableBytes),
              }, completedReport.unreadable.length)
            }}
          </dd>
          <dt>{{ t('common.elapsed') }}</dt>
          <dd>
            {{
              t('common.seconds', {
                seconds: formatSeconds(completedReport.elapsedMs),
              })
            }}
          </dd>
        </dl>

        <UnreadableRanges v-if="listedRanges.length > 0" :ranges="listedRanges" />
        <p v-if="completedReport.unreadable.length > MAX_LISTED_RANGES" class="wizard__note">
          {{
            t('badSectors.result.moreRanges', {
              shown: MAX_LISTED_RANGES,
              count: completedReport.unreadable.length,
            })
          }}
        </p>
        <p v-if="completedReport.unreadableRangesTruncated" class="wizard__note">
          {{ t('badSectors.result.truncated', { count: completedReport.unreadable.length }) }}
        </p>

        <p class="tool__next">
          {{
            completedReport.unreadableBytes === 0
              ? t('badSectors.result.nextClean')
              : t('badSectors.result.nextFound')
          }}
        </p>
      </section>
    </div>
  </section>
</template>

<style scoped>
.bad-sectors__source {
  min-width: 0;
  margin: 0;
  padding: 0;
  border: none;
}
</style>
