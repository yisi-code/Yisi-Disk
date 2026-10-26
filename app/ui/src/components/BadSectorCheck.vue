<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import SourcePicker from './SourcePicker.vue'
import { useHostErrorText } from '../composables/useHostErrorText'
import { useReadScan } from '../composables/useReadScan'
import type { JobTarget } from '../services/host'
import { formatBytes } from '../utils/format'

/** 报告里最多列出的读不出区段数；其余只计入总数 */
const MAX_LISTED_RANGES = 20

const emit = defineEmits<{ exit: [] }>()

const { t, locale } = useI18n()
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
        ? t('badSectors.status.startingDisk')
        : t('badSectors.status.starting')
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
      return t('badSectors.status.cancelled')
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

function formatPosition(offset: number): string {
  return t('badSectors.result.positionValue', {
    position: formatBytes(offset),
    offset: offset.toLocaleString(locale.value),
  })
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

    <div class="bad-sectors__body">
      <p class="bad-sectors__intro">{{ t('badSectors.intro') }}</p>

      <details class="bad-sectors__guide" open>
        <summary class="bad-sectors__guide-summary">{{ t('badSectors.guide.title') }}</summary>
        <ul class="bad-sectors__guide-list">
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
          <button
            v-else
            class="wizard__button wizard__button--secondary"
            type="button"
            :disabled="status !== 'running'"
            @click="handleCancel"
          >
            {{ status === 'cancelling' ? t('badSectors.cancelling') : t('badSectors.cancel') }}
          </button>
        </div>
      </form>

      <div v-if="status !== 'idle'" class="wizard__job" aria-live="polite">
        <progress class="wizard__progress" :value="done" :max="total || 1"></progress>
        <p class="wizard__status">{{ statusText }}</p>
        <p v-if="isUnresponsive" class="wizard__error" role="alert">
          {{ t('badSectors.unresponsive') }}
        </p>
        <p v-if="failure" class="wizard__error" role="alert">{{ formatHostError(failure) }}</p>
      </div>

      <section
        v-if="completedReport"
        class="bad-sectors__report"
        aria-labelledby="bad-sectors-result-title"
      >
        <h3 id="bad-sectors-result-title" class="bad-sectors__report-title">
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
              })
            }}
          </dd>
          <dt>{{ t('badSectors.result.elapsed') }}</dt>
          <dd>
            {{
              t('badSectors.result.elapsedValue', {
                seconds: (completedReport.elapsedMs / 1000).toFixed(1),
              })
            }}
          </dd>
        </dl>

        <table v-if="listedRanges.length > 0" class="wizard__table">
          <caption class="wizard__caption">{{ t('badSectors.result.rangesCaption') }}</caption>
          <thead>
            <tr>
              <th scope="col">{{ t('badSectors.result.start') }}</th>
              <th scope="col" class="wizard__number">{{ t('badSectors.result.length') }}</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="range in listedRanges" :key="range.offset">
              <td>{{ formatPosition(range.offset) }}</td>
              <td class="wizard__number">{{ formatBytes(range.length) }}</td>
            </tr>
          </tbody>
        </table>
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

        <p class="bad-sectors__next">
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
.bad-sectors__body {
  display: flex;
  flex-direction: column;
  gap: var(--space-md);
  margin-top: var(--space-md);
}

.bad-sectors__intro {
  margin: 0;
}

.bad-sectors__guide {
  padding: var(--space-sm) var(--space-md);
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-card);
}

.bad-sectors__guide-summary {
  font-weight: 600;
  cursor: pointer;
}

.bad-sectors__guide-list {
  display: flex;
  flex-direction: column;
  gap: var(--space-xs);
  margin: var(--space-sm) 0 0;
  padding-left: var(--space-lg);
}

.bad-sectors__source {
  min-width: 0;
  margin: 0;
  padding: 0;
  border: none;
}

.bad-sectors__report {
  display: flex;
  flex-direction: column;
  gap: var(--space-sm);
}

.bad-sectors__report-title {
  margin: 0;
  font-size: var(--font-size-section-title);
  font-weight: 600;
}

.bad-sectors__next {
  margin: 0;
  padding: var(--space-sm) var(--space-md);
  border-left: 4px solid var(--color-warning);
  background: var(--color-bg);
  border-radius: var(--space-xs);
}
</style>
