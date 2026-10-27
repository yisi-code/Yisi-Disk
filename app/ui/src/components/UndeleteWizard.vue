<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import PartitionFiles from './PartitionFiles.vue'
import PartitionPicker from './PartitionPicker.vue'
import SourcePicker from './SourcePicker.vue'
import { useBrowse } from '../composables/useBrowse'
import { useHostErrorText } from '../composables/useHostErrorText'
import { usePartitionText } from '../composables/usePartitionText'
import type { JobTarget, PartitionRef } from '../services/host'
import { formatBytes } from '../utils/format'

const STEPS = ['source', 'partition', 'browse'] as const
type WizardStep = (typeof STEPS)[number]

const emit = defineEmits<{ exit: [] }>()

const { t, locale } = useI18n()
const { formatHostError } = useHostErrorText()
const { filesystemName, tableNote, sourceName, unbrowsableReason } = usePartitionText()
const browse = useBrowse()

const titleElement = ref<HTMLHeadingElement | null>(null)
const step = ref<WizardStep>('source')
const sourceKind = ref<JobTarget['kind']>('image')
const imagePath = ref('')
const selectedDisk = ref<number | null>(null)
const selectedPartition = ref<number | null>(null)
/** 复制进行中（由 PartitionFiles 给出）：不能返回或离开 */
const isCopying = ref(false)

const stepIndex = computed(() => STEPS.indexOf(step.value))

const target = computed<JobTarget | null>(() => {
  if (sourceKind.value === 'image') {
    const path = imagePath.value.trim()
    return path === '' ? null : { kind: 'image', path }
  }
  return selectedDisk.value === null ? null : { kind: 'disk', diskNumber: selectedDisk.value }
})

const partitions = computed(() => browse.opened.value?.partitions ?? [])
const currentPartition = computed(
  () => partitions.value.find((partition) => partition.number === selectedPartition.value) ?? null,
)
const isSourceDisk = computed(() => browse.target.value?.kind === 'disk')

const canGoNext = computed(() => {
  switch (step.value) {
    case 'source':
      return target.value !== null && !browse.isTransitioning.value
    case 'partition':
      return currentPartition.value?.browsable === true && !browse.isScanning.value
    case 'browse':
      return false
  }
  return false
})

/** 当前分区扫描已删除项的结果（只有 NTFS 有） */
const currentScan = computed(() =>
  currentPartition.value === null
    ? null
    : (browse.scanResults.value.get(currentPartition.value.number) ?? null),
)
const needsScanHint = computed(() => partitions.value.some((partition) => partition.needsScan))

const partitionRef = computed<PartitionRef | null>(() =>
  currentPartition.value === null
    ? null
    : {
        number: currentPartition.value.number,
        offset: currentPartition.value.offset,
        length: currentPartition.value.length,
      },
)

const scanStatusText = computed(() =>
  browse.scanTotal.value > 0
    ? t('undelete.scan.running', {
        done: formatBytes(browse.scanDone.value),
        total: formatBytes(browse.scanTotal.value),
        percent: browse.scanPercent.value,
      })
    : t('undelete.scan.starting'),
)

const sourceText = computed(() => sourceName(browse.target.value))

const openingText = computed(() =>
  target.value?.kind === 'disk' ? t('undelete.openingDisk') : t('undelete.opening'),
)

onMounted(() => {
  titleElement.value?.focus()
})

async function handleExit() {
  if (isCopying.value) {
    return
  }
  if (browse.isOpen.value) {
    await browse.close()
  }
  emit('exit')
}

async function goBack() {
  const previous = STEPS[stepIndex.value - 1]
  if (previous === undefined || isCopying.value || browse.isScanning.value) {
    return
  }
  // 回到来源这一步就关闭浏览：换了源要重新读分区表
  if (previous === 'source' && browse.isOpen.value) {
    await browse.close()
  }
  step.value = previous
}

async function handleSubmit() {
  if (step.value === 'source') {
    await openSource()
  } else if (step.value === 'partition' && canGoNext.value) {
    await enterPartition()
  }
}

/** NTFS 分区先扫一遍 MFT 找出已删除的项（扫过的直接进入），被取消或失败时留在分区这一步 */
async function enterPartition() {
  const partition = currentPartition.value
  if (partition === null) {
    return
  }
  if (partition.needsScan && !browse.scanResults.value.has(partition.number)) {
    const result = await browse.scan(partition.number)
    if (result === null || step.value !== 'partition') {
      return
    }
  }
  step.value = 'browse'
}

async function openSource() {
  const current = target.value
  if (current === null || !canGoNext.value) {
    return
  }
  await browse.open(current)
  if (browse.isOpen.value) {
    selectedPartition.value =
      partitions.value.find((partition) => partition.browsable)?.number ?? null
    step.value = 'partition'
  }
}
</script>

<template>
  <section
    class="wizard"
    :class="{ 'wizard--fill': step === 'browse' }"
    aria-labelledby="undelete-wizard-title"
  >
    <header class="wizard__header">
      <button
        class="wizard__button wizard__button--secondary"
        type="button"
        :disabled="isCopying"
        @click="handleExit"
      >
        {{ t('nav.back') }}
      </button>
      <h2 id="undelete-wizard-title" ref="titleElement" class="wizard__title" tabindex="-1">
        {{ t('features.undelete.name') }}
      </h2>
    </header>

    <form class="wizard__form" @submit.prevent="handleSubmit">
      <div class="wizard__stepbar">
        <ol class="wizard__steps">
          <li
            v-for="(name, index) in STEPS"
            :key="name"
            class="wizard__step"
            :class="{
              'wizard__step--current': name === step,
              'wizard__step--done': index < stepIndex,
            }"
            :aria-current="name === step ? 'step' : undefined"
          >
            <span class="wizard__step-number">{{ index + 1 }}</span>
            {{ t(`undelete.steps.${name}`) }}
          </li>
        </ol>
        <div class="wizard__nav">
          <button
            v-if="stepIndex > 0"
            class="wizard__button wizard__button--secondary"
            type="button"
            :disabled="isCopying || browse.isScanning.value"
            @click="goBack"
          >
            {{ t('carve.previous') }}
          </button>
          <button v-if="step !== 'browse'" class="wizard__button" type="submit" :disabled="!canGoNext">
            {{ t('carve.next') }}
          </button>
        </div>
      </div>

      <div v-if="step === 'source'" class="wizard__panel">
        <SourcePicker
          v-model:kind="sourceKind"
          v-model:image-path="imagePath"
          v-model:disk="selectedDisk"
          :disk-hint="t('undelete.diskHint')"
        />
        <p v-if="browse.status.value === 'starting'" class="wizard__note" aria-live="polite">
          {{ openingText }}
        </p>
        <p v-if="browse.failure.value" class="wizard__error" role="alert">
          {{ formatHostError(browse.failure.value) }}
        </p>
      </div>

      <div v-else-if="step === 'partition'" class="wizard__panel">
        <p class="wizard__note">
          {{
            t('undelete.partition.summary', {
              source: sourceText,
              scheme: t(`undelete.partition.scheme.${browse.opened.value?.scheme ?? 'none'}`),
              size: formatBytes(browse.opened.value?.size ?? 0),
            })
          }}
        </p>
        <ul v-if="browse.opened.value?.notes.length" class="undelete-wizard__notes">
          <li v-for="note in browse.opened.value.notes" :key="note">{{ tableNote(note) }}</li>
        </ul>
        <PartitionPicker
          v-model="selectedPartition"
          :partitions="partitions"
          :caption="t('undelete.partition.caption')"
          :is-selectable="(partition) => partition.browsable"
          :note="(partition) => (partition.browsable ? '' : unbrowsableReason(partition.filesystem))"
          :disabled="browse.isScanning.value"
        />
        <p v-if="!partitions.some((partition) => partition.browsable)" class="wizard__note">
          {{ t('undelete.partition.none') }}
        </p>
        <p v-if="needsScanHint" class="wizard__hint">{{ t('undelete.partition.scanHint') }}</p>
        <div v-if="browse.isScanning.value" class="wizard__job" aria-live="polite">
          <progress
            class="wizard__progress"
            :value="browse.scanDone.value"
            :max="browse.scanTotal.value || 1"
          ></progress>
          <p class="wizard__status">{{ scanStatusText }}</p>
          <div>
            <button
              class="wizard__button wizard__button--secondary"
              type="button"
              :disabled="browse.isCancellingScan.value"
              @click="browse.stopScan"
            >
              {{ browse.isCancellingScan.value ? t('undelete.scan.cancelling') : t('undelete.scan.cancel') }}
            </button>
          </div>
        </div>
        <p v-if="browse.scanFailure.value" class="wizard__error" role="alert">
          {{ t('undelete.scan.failed', { reason: formatHostError(browse.scanFailure.value) }) }}
        </p>
      </div>

      <div v-else-if="currentPartition && browse.jobId.value" class="wizard__panel wizard__fill">
        <dl class="wizard__details">
          <dt>{{ t('undelete.browse.source') }}</dt>
          <dd class="wizard__path-text">{{ sourceText }}</dd>
          <dt>{{ t('undelete.browse.partition') }}</dt>
          <dd>
            {{
              t('undelete.browse.partitionValue', {
                number: currentPartition.number,
                label: currentPartition.label ?? '',
                filesystem: filesystemName(currentPartition.filesystem),
                size: formatBytes(currentPartition.length),
              })
            }}
          </dd>
        </dl>
        <p v-if="currentScan" class="wizard__note">
          {{
            t('undelete.scan.summary', {
              records: currentScan.records.toLocaleString(locale),
              deleted: currentScan.deleted.toLocaleString(locale),
            })
          }}
          <template v-if="currentScan.orphans > 0">
            {{ t('undelete.scan.orphans', { count: currentScan.orphans.toLocaleString(locale) }, currentScan.orphans) }}
          </template>
          <template v-if="currentScan.unreadable > 0">
            {{ t('undelete.scan.unreadable', { count: currentScan.unreadable.toLocaleString(locale) }, currentScan.unreadable) }}
          </template>
        </p>
        <p v-if="isSourceDisk" class="wizard__hint">{{ t('undelete.browse.liveHint') }}</p>
        <p v-if="browse.isUnresponsive.value" class="wizard__error" role="alert">
          {{ t('carve.run.unresponsive') }}
        </p>
        <p v-if="!browse.isOpen.value && browse.failure.value" class="wizard__error" role="alert">
          {{ formatHostError(browse.failure.value) }}
        </p>
        <div class="undelete-wizard__files wizard__fill">
          <PartitionFiles
            v-if="browse.target.value && partitionRef"
            v-model:busy="isCopying"
            :job-id="browse.jobId.value"
            :source="browse.target.value"
            :partition="currentPartition"
            :partition-ref="partitionRef"
            :sector-size="browse.opened.value?.sectorSize ?? 512"
            :can-browse="browse.isOpen.value"
          />
        </div>
      </div>
    </form>
  </section>
</template>

<style scoped>
.undelete-wizard__notes {
  margin: 0;
  padding-left: var(--space-lg);
  font-size: var(--font-size-small);
}

/* 列表与详情两栏都分标题、框体、底部三行，三行上下边对齐；列表要放名字、状态与时间，占得宽一些 */
.undelete-wizard__files {
  display: grid;
  grid-template-columns: minmax(0, 3fr) minmax(0, 2fr);
  grid-template-rows: auto 1fr auto;
  gap: var(--space-sm) var(--space-lg);
}
</style>
