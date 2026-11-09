<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import FeatureJump from './FeatureJump.vue'
import JobControls from './JobControls.vue'
import ReadHealthHint from './ReadHealthHint.vue'
import SourcePicker from './SourcePicker.vue'
import WizardSteps from './WizardSteps.vue'
import { useDiskSurvey, type SurveyFinding } from '../composables/useDiskSurvey'
import { useFeatureJump } from '../composables/useFeatureJump'
import { useHostErrorText } from '../composables/useHostErrorText'
import { usePartitionText } from '../composables/usePartitionText'
import { useSourceSelection } from '../composables/useSourceSelection'
import { isDeviceKind, type PartitionRef } from '../services/host'
import { formatBytes, formatCount } from '../utils/format'

const STEPS = ['source', 'check', 'result'] as const
type WizardStep = (typeof STEPS)[number]

const emit = defineEmits<{ exit: [] }>()

const { t, locale } = useI18n()
const { formatHostError } = useHostErrorText()
const { sourceName, partitionLine, tableNote } = usePartitionText()
const survey = useDiskSurvey()
const search = survey.search

const titleElement = ref<HTMLHeadingElement | null>(null)
const step = ref<WizardStep>('source')
const { sourceKind, imagePath, device, target } = useSourceSelection()
useFeatureJump(handleExit)

const stepIndex = computed(() => STEPS.indexOf(step.value))
const canStart = computed(() => step.value === 'source' && target.value !== null && !survey.isBusy.value)
const sourceText = computed(() => sourceName(survey.target.value))
const isDisk = computed(() => isDeviceKind(survey.target.value?.kind))

const statusText = computed(() => {
  switch (survey.phase.value) {
    case 'diagnosing':
      return isDisk.value ? t('survey.run.diagnosingDisk') : t('survey.run.diagnosing')
    case 'searching':
      if (search.status.value === 'starting') {
        return t('survey.run.searchStarting')
      }
      return t('survey.run.searching', search.progressParams.value)
  }
  return ''
})

/** 结论页顶部一句：源、分区表、大小，快速搜索的情况 */
const summaryText = computed(() => {
  const diagnosis = survey.diagnosis.value
  if (diagnosis === null) {
    return ''
  }
  const table = t('partition.picker.summary', {
    source: sourceText.value,
    scheme: t(`partition.picker.scheme.${diagnosis.scheme}`),
    size: formatBytes(diagnosis.size),
  })
  const summary = search.summary.value
  const searched =
    summary === null
      ? t('survey.result.notSearched')
      : summary.stoppedAt !== null
        ? t('survey.result.searchStopped', { percent: summary.size > 0 ? Math.floor((summary.stoppedAt / summary.size) * 100) : 0 })
        : t('survey.result.searched')
  return `${table}${t('survey.result.separator')}${searched}`
})

watch(survey.phase, (phase) => {
  if (phase === 'done') {
    step.value = 'result'
  }
})

onMounted(() => {
  titleElement.value?.focus()
})

async function handleExit() {
  if (!survey.isBusy.value) {
    emit('exit')
  }
}

function goBack() {
  if (!survey.isBusy.value && stepIndex.value > 0) {
    step.value = 'source'
  }
}

async function handleSubmit() {
  const current = target.value
  if (step.value === 'source' && current !== null && canStart.value) {
    step.value = 'check'
    await survey.run(current)
  }
}

async function runAgain() {
  const current = survey.target.value
  if (current !== null && !survey.isBusy.value) {
    step.value = 'check'
    await survey.run(current)
  }
}

function findingTitle(finding: SurveyFinding): string {
  const title = t(`survey.finding.${finding.kind}.title`, { count: formatCount(finding.count, locale.value) }, finding.count)
  return finding.partition === null
    ? title
    : t('survey.result.partitionTitle', { partition: partitionLine(finding.partition), title })
}

function findingDetail(finding: SurveyFinding): string {
  return t(
    `survey.finding.${finding.kind}.detail`,
    { count: formatCount(finding.count, locale.value), bytes: formatBytes(finding.count) },
    finding.count,
  )
}

function findingPartition(finding: SurveyFinding): PartitionRef | null {
  const partition = finding.partition
  return partition === null ? null : { number: partition.number, offset: partition.offset, length: partition.length }
}
</script>

<template>
  <section class="wizard" aria-labelledby="survey-title">
    <header class="wizard__header">
      <button
        class="wizard__button wizard__button--secondary"
        type="button"
        :disabled="survey.isBusy.value"
        @click="handleExit"
      >
        {{ t('nav.back') }}
      </button>
      <h2 id="survey-title" ref="titleElement" class="wizard__title" tabindex="-1">
        {{ t('features.survey.name') }}
      </h2>
    </header>

    <form class="wizard__form" @submit.prevent="handleSubmit">
      <div class="wizard__stepbar">
        <WizardSteps :steps="STEPS" :current="step" label-prefix="survey.steps" />
        <div class="wizard__nav">
          <button
            v-if="step === 'result'"
            class="wizard__button wizard__button--secondary"
            type="button"
            :disabled="survey.isBusy.value"
            @click="goBack"
          >
            {{ t('common.previous') }}
          </button>
          <button v-if="step === 'source'" class="wizard__button" type="submit" :disabled="!canStart">
            {{ t('survey.start') }}
          </button>
          <button
            v-else-if="step === 'result'"
            class="wizard__button wizard__button--secondary"
            type="button"
            :disabled="survey.isBusy.value"
            @click="runAgain"
          >
            {{ t('survey.again') }}
          </button>
        </div>
      </div>

      <div v-if="step === 'source'" class="wizard__panel">
        <p class="wizard__note">{{ t('survey.intro') }}</p>
        <SourcePicker
          v-model:kind="sourceKind"
          v-model:image-path="imagePath"
          v-model:device="device"
          :disk-hint="t('survey.diskHint')"
        />
      </div>

      <div v-else-if="step === 'check'" class="wizard__panel">
        <dl class="wizard__details">
          <dt>{{ t('common.source') }}</dt>
          <dd class="wizard__path-text">{{ sourceText }}</dd>
        </dl>
        <div class="wizard__job" aria-live="polite">
          <progress
            v-if="survey.phase.value === 'searching'"
            class="wizard__progress"
            :value="search.done.value"
            :max="search.total.value || 1"
          ></progress>
          <p class="wizard__status">{{ statusText }}</p>
          <JobControls
            v-if="search.isBusy.value"
            :status="search.status.value"
            keeps-result
            @cancel="survey.stop"
          />
          <ReadHealthHint :health="search.readHealth.value" :source-is-disk="isDisk" context="scan" />
          <p v-if="search.isUnresponsive.value" class="wizard__error" role="alert">
            {{ t('common.unresponsive') }}
          </p>
        </div>
        <template v-if="survey.phase.value === 'failed' && survey.failure.value">
          <p class="wizard__error" role="alert">
            {{ t('survey.run.failed', { reason: formatHostError(survey.failure.value) }) }}
          </p>
          <div class="wizard__retry">
            <button class="wizard__button" type="button" @click="runAgain">{{ t('survey.again') }}</button>
          </div>
          <FeatureJump
            :features="['badSectors', 'image', 'carve']"
            :target="survey.target.value"
            :lead="t('survey.run.failedLead')"
          />
        </template>
      </div>

      <div v-else-if="survey.diagnosis.value" class="wizard__panel">
        <p class="wizard__note">{{ summaryText }}</p>
        <p class="wizard__hint">{{ t('survey.result.readOnly') }}</p>
        <p v-if="search.failure.value" class="wizard__error" role="alert">
          {{ t('survey.result.searchFailed', { reason: formatHostError(search.failure.value) }) }}
        </p>
        <ul v-if="survey.diagnosis.value.notes.length" class="wizard__list">
          <li v-for="note in survey.diagnosis.value.notes" :key="note">{{ tableNote(note) }}</li>
        </ul>

        <section
          v-for="(finding, index) in survey.findings.value"
          :key="`${finding.kind}-${finding.partition?.number ?? index}`"
          class="survey__finding"
          :class="`survey__finding--${finding.level}`"
          :aria-labelledby="`survey-finding-${index}`"
        >
          <h3 :id="`survey-finding-${index}`" class="wizard__section-title survey__title">
            <span class="survey__level">{{ t(`survey.level.${finding.level}`) }}</span>
            {{ findingTitle(finding) }}
          </h3>
          <p class="survey__detail">{{ findingDetail(finding) }}</p>
          <FeatureJump
            v-if="finding.features.length > 0"
            :features="finding.features"
            :target="survey.target.value"
            :partition="findingPartition(finding)"
            :search-mode="finding.searchMode"
          />
        </section>
      </div>
    </form>
  </section>
</template>

<style scoped>
.survey__finding {
  display: flex;
  flex-direction: column;
  gap: var(--space-sm);
  padding: var(--space-md);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-left: 4px solid var(--color-border);
  border-radius: var(--radius-card);
}

.survey__finding--danger {
  border-left-color: var(--color-danger);
}

.survey__finding--warning {
  border-left-color: var(--color-warning);
}

.survey__finding--ok {
  border-left-color: var(--color-success);
}

.survey__title {
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  gap: var(--space-sm);
}

/* 轻重写成文字标签，不只靠左侧色条的颜色 */
.survey__level {
  flex: none;
  padding: 0 var(--space-sm);
  font-size: var(--font-size-small);
  font-weight: normal;
  border: 1px solid currentColor;
  border-radius: var(--radius-card);
}

.survey__finding--danger .survey__level {
  color: var(--color-danger);
}

.survey__finding--warning .survey__level {
  color: var(--color-warning);
}

.survey__finding--ok .survey__level {
  color: var(--color-success);
}

.survey__finding--info .survey__level {
  color: var(--color-text-muted);
}

.survey__detail {
  margin: 0;
}
</style>
