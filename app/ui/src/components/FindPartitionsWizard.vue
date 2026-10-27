<script setup lang="ts">
import { computed, onMounted, ref, useId, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import MftScanProgress from './MftScanProgress.vue'
import PartitionFiles from './PartitionFiles.vue'
import SourcePicker from './SourcePicker.vue'
import WizardSteps from './WizardSteps.vue'
import JobControls from './JobControls.vue'
import ReadHealthHint from './ReadHealthHint.vue'
import { useBrowse } from '../composables/useBrowse'
import { useHostErrorText } from '../composables/useHostErrorText'
import { usePartitionText } from '../composables/usePartitionText'
import { usePartitionSearch } from '../composables/usePartitionSearch'
import {
  toHostError,
  type FoundPartition,
  type HostError,
  type JobTarget,
  type PartitionInfo,
  type PartitionRef,
} from '../services/host'
import { formatBytes, formatSeconds } from '../utils/format'

const STEPS = ['source', 'search', 'results'] as const
type WizardStep = (typeof STEPS)[number]

const emit = defineEmits<{ exit: [] }>()

const { t } = useI18n()
const { formatHostError } = useHostErrorText()
const { filesystemName, sourceName, unbrowsableReason } = usePartitionText()
const search = usePartitionSearch()
const browse = useBrowse()
const domPrefix = useId()

const titleElement = ref<HTMLHeadingElement | null>(null)
const step = ref<WizardStep>('source')
const sourceKind = ref<JobTarget['kind']>('image')
const imagePath = ref('')
const selectedDisk = ref<number | null>(null)
/** 选中的候选分区在结果里的下标 */
const selectedIndex = ref<number | null>(null)
/** 选中的候选分区在浏览会话里按位置打开后的信息；NTFS 扫完 MFT 才给出 */
const openedPartition = ref<PartitionInfo | null>(null)
const isOpeningPartition = ref(false)
const openError = ref<HostError | null>(null)
/** 复制进行中（由 PartitionFiles 给出）：不能返回、离开或换分区 */
const isCopying = ref(false)
/** 每次打开候选分区加一，晚到的旧答复丢弃 */
let openSerial = 0

const stepIndex = computed(() => STEPS.indexOf(step.value))

const target = computed<JobTarget | null>(() => {
  if (sourceKind.value === 'image') {
    const path = imagePath.value.trim()
    return path === '' ? null : { kind: 'image', path }
  }
  return selectedDisk.value === null ? null : { kind: 'disk', diskNumber: selectedDisk.value }
})

const candidates = computed(() => search.summary.value?.candidates ?? [])
const selectedCandidate = computed(() =>
  selectedIndex.value === null ? null : (candidates.value[selectedIndex.value] ?? null),
)

/** 复制时交给引擎的分区位置：编号是按位置打开时引擎给的，引擎据此只核对范围 */
const partitionRef = computed<PartitionRef | null>(() => {
  const partition = openedPartition.value
  const candidate = selectedCandidate.value
  if (partition === null || candidate === null) {
    return null
  }
  return {
    number: partition.number,
    offset: partition.offset,
    length: partition.length,
  }
})

/** 打开分区、扫描 MFT、复制进行中时不能换分区，也不能返回 */
const isPartitionBusy = computed(
  () => isOpeningPartition.value || browse.isScanning.value || isCopying.value,
)

const sourceText = computed(() => sourceName(search.target.value))

const statusText = computed(() => {
  switch (search.status.value) {
    case 'starting':
      return search.target.value?.kind === 'disk'
        ? t('common.startingDisk')
        : t('findPartitions.status.starting')
    case 'running':
    case 'cancelling':
      if (search.isPaused.value) {
        return t('findPartitions.status.paused', {
          done: formatBytes(search.done.value),
          total: formatBytes(search.total.value),
          percent: search.percent.value,
        })
      }
      return t('findPartitions.status.running', {
        done: formatBytes(search.done.value),
        total: formatBytes(search.total.value),
        percent: search.percent.value,
        rate: search.bytesPerSecond.value === null ? '-' : formatBytes(search.bytesPerSecond.value),
      })
    case 'completed':
      return t('findPartitions.status.completed')
    case 'failed':
      return t('findPartitions.status.failed')
    case 'cancelled':
      return t('findPartitions.status.cancelled')
  }
  return ''
})

const summaryText = computed(() => {
  const summary = search.summary.value
  if (summary === null) {
    return ''
  }
  const parts = [
    t('findPartitions.results.summary', { count: summary.candidates.length }, summary.candidates.length),
    t('findPartitions.results.table', {
      scheme: t(`findPartitions.results.scheme.${summary.scheme}`),
    }),
    t('findPartitions.results.elapsed', { seconds: formatSeconds(summary.elapsedMs) }),
  ]
  if (summary.unreadableBytes > 0) {
    parts.push(t('findPartitions.results.unreadable', { size: formatBytes(summary.unreadableBytes) }))
  }
  if (summary.candidatesTruncated) {
    parts.push(t('findPartitions.results.truncated', { count: summary.candidates.length }))
  }
  return parts.join(t('findPartitions.results.separator'))
})

watch(search.status, (status) => {
  if (status === 'completed' && step.value === 'search') {
    void enterResults()
  }
})

watch(selectedIndex, () => {
  void openSelected()
})

onMounted(() => {
  titleElement.value?.focus()
})

async function handleExit() {
  if (search.isBusy.value || isPartitionBusy.value) {
    return
  }
  await closeBrowse()
  emit('exit')
}

async function goBack() {
  const previous = STEPS[stepIndex.value - 1]
  if (previous === undefined || search.isBusy.value || isPartitionBusy.value) {
    return
  }
  // 结果页回到来源：关闭浏览，换了源要重新搜索
  await closeBrowse()
  step.value = 'source'
}

/** 来源这一步、搜索没跑完、结果页上都是「开始 / 重新搜索」 */
async function handleSubmit() {
  const current = target.value
  if (current === null || search.isBusy.value || isPartitionBusy.value) {
    return
  }
  await closeBrowse()
  step.value = 'search'
  await search.start(current)
}

/** 搜索完成：进入结果页，有找到分区时打开浏览会话（物理磁盘这时可能再请求一次提权） */
async function enterResults() {
  step.value = 'results'
  const source = search.target.value
  if (source !== null && candidates.value.length > 0) {
    await browse.open(source)
  }
}

async function closeBrowse() {
  openSerial += 1
  selectedIndex.value = null
  openedPartition.value = null
  openError.value = null
  isOpeningPartition.value = false
  if (browse.isOpen.value) {
    await browse.close()
  }
}

/** 选中的候选分区在浏览会话里按位置打开；NTFS 先扫一遍 MFT（扫过的直接进入），被停下或失败时留在占位提示 */
async function openSelected() {
  const serial = ++openSerial
  openedPartition.value = null
  openError.value = null
  const candidate = selectedCandidate.value
  if (candidate === null) {
    return
  }
  isOpeningPartition.value = true
  let partition: PartitionInfo | null
  try {
    partition = await browse.openRange(candidate.offset, candidate.length, candidate.boot)
  } catch (error) {
    if (serial === openSerial) {
      openError.value = toHostError(error)
    }
    return
  } finally {
    if (serial === openSerial) {
      isOpeningPartition.value = false
    }
  }
  if (serial !== openSerial || partition === null) {
    return
  }
  if (partition.browsable && partition.needsScan && !browse.scanResults.value.has(partition.number)) {
    const result = await browse.scan(partition.number)
    if (serial !== openSerial || result === null) {
      return
    }
  }
  openedPartition.value = partition
}

function candidateName(candidate: FoundPartition): string {
  return t('findPartitions.list.name', {
    filesystem: filesystemName(candidate.filesystem),
    size: formatBytes(candidate.length),
  })
}

/** 候选分区的说明标签：与分区表的关系，以及靠备份找到、与别的候选重叠、超出末尾 */
function candidateTags(candidate: FoundPartition): { kind: string; text: string }[] {
  const tags: { kind: string; text: string }[] = [
    {
      kind: candidate.relation,
      text: t(`findPartitions.list.relation.${candidate.relation}`, {
        number: candidate.tablePartition ?? '',
      }),
    },
  ]
  if (candidate.foundBy === 'backup') {
    tags.push({ kind: 'backup', text: t('findPartitions.list.backup') })
  }
  if (candidate.overlapsCandidate) {
    tags.push({ kind: 'overlapsCandidate', text: t('findPartitions.list.overlapsCandidate') })
  }
  if (candidate.truncated) {
    tags.push({ kind: 'truncated', text: t('findPartitions.list.truncated') })
  }
  return tags
}
</script>

<template>
  <section
    class="wizard"
    :class="{ 'wizard--fill': step === 'results' }"
    aria-labelledby="find-partitions-title"
  >
    <header class="wizard__header">
      <button
        class="wizard__button wizard__button--secondary"
        type="button"
        :disabled="search.isBusy.value || isPartitionBusy"
        @click="handleExit"
      >
        {{ t('nav.back') }}
      </button>
      <h2 id="find-partitions-title" ref="titleElement" class="wizard__title" tabindex="-1">
        {{ t('features.findPartitions.name') }}
      </h2>
    </header>

    <form class="wizard__form" @submit.prevent="handleSubmit">
      <div class="wizard__stepbar">
        <WizardSteps :steps="STEPS" :current="step" label-prefix="findPartitions.steps" />
        <div class="wizard__nav">
          <button
            v-if="stepIndex > 0"
            class="wizard__button wizard__button--secondary"
            type="button"
            :disabled="search.isBusy.value || isPartitionBusy"
            @click="goBack"
          >
            {{ t('common.previous') }}
          </button>
          <button v-if="step === 'source'" class="wizard__button" type="submit" :disabled="target === null">
            {{ t('findPartitions.start') }}
          </button>
          <JobControls
            v-else-if="search.isBusy.value"
            :status="search.status.value"
            :pause="{ isPaused: search.isPaused.value, isToggling: search.isTogglingPause.value }"
            @pause="search.pause"
            @resume="search.resume"
            @cancel="search.cancel"
          />
          <button
            v-else
            class="wizard__button"
            type="submit"
            :disabled="target === null || isPartitionBusy"
          >
            {{ t('findPartitions.restart') }}
          </button>
        </div>
      </div>

      <div v-if="step === 'source'" class="wizard__panel">
        <p class="wizard__note">{{ t('findPartitions.intro') }}</p>
        <SourcePicker
          v-model:kind="sourceKind"
          v-model:image-path="imagePath"
          v-model:disk="selectedDisk"
          :disk-hint="t('findPartitions.diskHint')"
        />
        <p class="wizard__hint">{{ t('findPartitions.results.readOnly') }}</p>
      </div>

      <div v-else-if="step === 'search'" class="wizard__panel">
        <dl class="wizard__details">
          <dt>{{ t('common.source') }}</dt>
          <dd class="wizard__path-text">{{ sourceText }}</dd>
        </dl>
        <div class="wizard__job" aria-live="polite">
          <progress
            class="wizard__progress"
            :value="search.done.value"
            :max="search.total.value || 1"
          ></progress>
          <p class="wizard__status">{{ statusText }}</p>
          <ReadHealthHint
            :health="search.readHealth.value"
            :source-is-disk="search.target.value?.kind === 'disk'"
            context="scan"
          />
          <p v-if="search.isUnresponsive.value" class="wizard__error" role="alert">
            {{ t('common.unresponsive') }}
          </p>
          <p v-if="search.failure.value" class="wizard__error" role="alert">
            {{ formatHostError(search.failure.value) }}
          </p>
        </div>
      </div>

      <div v-else class="wizard__panel wizard__fill">
        <dl class="wizard__details">
          <dt>{{ t('common.source') }}</dt>
          <dd class="wizard__path-text">{{ sourceText }}</dd>
        </dl>
        <p class="wizard__note">{{ summaryText }}</p>
        <p class="wizard__hint wizard__hint--warning">{{ t('findPartitions.results.readOnly') }}</p>
        <p v-if="browse.status.value === 'starting'" class="wizard__note" aria-live="polite">
          {{
            search.target.value?.kind === 'disk'
              ? t('common.startingDisk')
              : t('findPartitions.results.opening')
          }}
        </p>
        <p v-if="browse.isUnresponsive.value" class="wizard__error" role="alert">
          {{ t('common.unresponsive') }}
        </p>
        <p v-if="!browse.isOpen.value && browse.failure.value" class="wizard__error" role="alert">
          {{ formatHostError(browse.failure.value) }}
        </p>

        <p v-if="candidates.length === 0" class="wizard__note">{{ t('findPartitions.results.none') }}</p>
        <div v-else class="find-partitions__results wizard__fill">
          <section class="find-partitions__pane" :aria-labelledby="`${domPrefix}-list`">
            <h3 :id="`${domPrefix}-list`" class="find-partitions__title">
              {{ t('findPartitions.list.title') }}
            </h3>
            <ul class="find-partitions__list">
              <li
                v-for="(candidate, index) in candidates"
                :key="`${candidate.offset}-${candidate.length}-${candidate.filesystem}`"
              >
                <label
                  class="find-partitions__item"
                  :class="{ 'find-partitions__item--selected': index === selectedIndex }"
                >
                  <input
                    v-model="selectedIndex"
                    type="radio"
                    :name="`${domPrefix}-candidate`"
                    :value="index"
                    :disabled="isPartitionBusy || !browse.isOpen.value"
                  />
                  <span class="find-partitions__item-body">
                    <span class="find-partitions__item-name">
                      {{ candidateName(candidate) }}
                      <span class="find-partitions__item-label">
                        {{ candidate.label ?? t('findPartitions.list.noLabel') }}
                      </span>
                    </span>
                    <span class="find-partitions__item-start">
                      {{ t('findPartitions.list.start', { offset: formatBytes(candidate.offset) }) }}
                    </span>
                    <span class="find-partitions__tags">
                      <span
                        v-for="tag in candidateTags(candidate)"
                        :key="tag.kind"
                        class="find-partitions__tag"
                        :class="`find-partitions__tag--${tag.kind}`"
                      >
                        {{ tag.text }}
                      </span>
                    </span>
                  </span>
                </label>
              </li>
            </ul>
            <p class="wizard__hint">{{ t('findPartitions.list.hint') }}</p>
          </section>

          <PartitionFiles
            v-if="openedPartition?.browsable && partitionRef && browse.jobId.value && search.target.value"
            v-model:busy="isCopying"
            :job-id="browse.jobId.value"
            :source="search.target.value"
            :partition="openedPartition"
            :partition-ref="partitionRef"
            :boot="selectedCandidate?.boot ?? 'primary'"
            :sector-size="search.summary.value?.sectorSize ?? 512"
            :can-browse="browse.isOpen.value"
          />
          <section
            v-else
            class="find-partitions__pane find-partitions__pane--wide"
            :aria-labelledby="`${domPrefix}-files`"
          >
            <h3 :id="`${domPrefix}-files`" class="find-partitions__title">
              {{ t('findPartitions.files.title') }}
            </h3>
            <div class="find-partitions__box" aria-live="polite">
              <p v-if="selectedCandidate === null" class="wizard__note">
                {{ t('findPartitions.files.pick') }}
              </p>
              <p v-else-if="isOpeningPartition" class="wizard__note">
                {{ t('findPartitions.files.opening') }}
              </p>
              <MftScanProgress v-else-if="browse.isScanning.value" :browse="browse" />
              <template v-else>
                <p v-if="openError" class="wizard__error" role="alert">
                  {{ t('findPartitions.files.openFailed', { reason: formatHostError(openError) }) }}
                </p>
                <p v-else-if="openedPartition" class="wizard__note">
                  {{ t('findPartitions.files.unbrowsable', { reason: unbrowsableReason(openedPartition.filesystem) }) }}
                </p>
                <p v-else-if="browse.scanFailure.value" class="wizard__error" role="alert">
                  {{ t('partitionFiles.scan.failed', { reason: formatHostError(browse.scanFailure.value) }) }}
                </p>
                <p v-else class="wizard__note">{{ t('findPartitions.files.scanCancelled') }}</p>
                <div v-if="!openedPartition">
                  <button
                    class="wizard__button wizard__button--secondary"
                    type="button"
                    :disabled="!browse.isOpen.value"
                    @click="openSelected"
                  >
                    {{ t('findPartitions.files.retry') }}
                  </button>
                </div>
              </template>
            </div>
            <!-- 底部一行留空，与左右两栏的提示行对齐 -->
            <div></div>
          </section>
        </div>
      </div>
    </form>
  </section>
</template>

<style scoped>
/*
 * 三栏：找到的分区、分区里的文件、详情。各分标题、框体、底部三行（CSS subgrid），三行上下边对齐；
 * 后两栏由 PartitionFiles 排进来，还没选分区时是一个横跨两栏的占位框
 */
.find-partitions__results {
  display: grid;
  grid-template-columns: minmax(240px, 1.2fr) minmax(0, 2.4fr) minmax(0, 1.6fr);
  grid-template-rows: auto 1fr auto;
  gap: var(--space-sm) var(--space-lg);
}

.find-partitions__pane {
  display: grid;
  grid-row: span 3;
  grid-template-rows: subgrid;
  min-width: 0;
}

.find-partitions__pane--wide {
  grid-column: 2 / -1;
}

.find-partitions__title {
  align-self: center;
  margin: 0;
  font-size: var(--font-size-body);
  font-weight: 600;
}

/* 列表框与占位框都不按内容撑高，与右边的文件列表框等高，内容超出时在框里滚动 */
.find-partitions__list,
.find-partitions__box {
  min-height: var(--box-min-height);
  margin: 0;
  overflow-y: auto;
  border: 1px solid var(--color-border);
  border-radius: var(--radius-card);
  contain: size;
}

.find-partitions__list {
  padding: 0;
  list-style: none;
}

.find-partitions__box {
  display: flex;
  flex-direction: column;
  gap: var(--space-sm);
  padding: var(--space-sm);
}

.find-partitions__item {
  display: flex;
  align-items: flex-start;
  gap: var(--space-sm);
  padding: var(--space-sm);
  border-bottom: 1px solid var(--color-border);
  cursor: pointer;
}

.find-partitions__item--selected {
  background: color-mix(in srgb, var(--color-accent) 16%, transparent);
}

.find-partitions__item-body {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}

.find-partitions__item-name {
  font-weight: 600;
  overflow-wrap: anywhere;
}

.find-partitions__item-label {
  margin-left: var(--space-xs);
  font-weight: normal;
}

.find-partitions__item-start {
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
}

.find-partitions__tags {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-xs);
}

.find-partitions__tag {
  padding: 0 var(--space-xs);
  font-size: var(--font-size-small);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-control);
}

.find-partitions__tag--lost {
  border-color: var(--color-warning);
}
</style>
