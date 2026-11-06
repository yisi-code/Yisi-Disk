<script setup lang="ts">
import { computed, onMounted, ref, useId, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import FoundPartitionItem from './FoundPartitionItem.vue'
import DeletedScanProgress from './DeletedScanProgress.vue'
import PartitionFiles from './PartitionFiles.vue'
import SourcePicker from './SourcePicker.vue'
import WizardSteps from './WizardSteps.vue'
import JobControls from './JobControls.vue'
import ReadHealthHint from './ReadHealthHint.vue'
import WritePlanConfirm from './WritePlanConfirm.vue'
import { useBrowse } from '../composables/useBrowse'
import { useHostErrorText } from '../composables/useHostErrorText'
import { usePartitionText } from '../composables/usePartitionText'
import { usePartitionSearch } from '../composables/usePartitionSearch'
import { useSourceSelection } from '../composables/useSourceSelection'
import { useWritePlan } from '../composables/useWritePlan'
import { useWritePlanText } from '../composables/useWritePlanText'
import {
  canWritePlan,
  isDeviceKind,
  toHostError,
  type HostError,
  type PartitionInfo,
  type PartitionRef,
  type SearchMode,
} from '../services/host'
import { formatBytes, formatSeconds } from '../utils/format'

const STEPS = ['source', 'search', 'results'] as const
type WizardStep = (typeof STEPS)[number]

const emit = defineEmits<{ exit: [] }>()

const { t } = useI18n()
const { formatHostError } = useHostErrorText()
const { sourceName, unbrowsableReason } = usePartitionText()
const search = usePartitionSearch()
const browse = useBrowse()
const writePlan = useWritePlan()
const { writeActionText, writeOutcomeText } = useWritePlanText()
const domPrefix = useId()

const titleElement = ref<HTMLHeadingElement | null>(null)
const step = ref<WizardStep>('source')
const { sourceKind, imagePath, device, target } = useSourceSelection()
/** 下一次搜索怎么搜：来源这一步选，结果页的「深度搜索」改成 deep */
const searchMode = ref<SearchMode>('quick')
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

/** 打开分区、扫描 MFT、复制、生成或执行写入计划进行中时不能换分区，也不能返回 */
const isPartitionBusy = computed(
  () =>
    isOpeningPartition.value ||
    browse.isScanning.value ||
    isCopying.value ||
    writePlan.isPreparing.value ||
    writePlan.isExecuting.value,
)

/** 镜像文件与整块磁盘可以写回分区表；卷上没有分区表 */
const canWrite = computed(() => canWritePlan(search.target.value))

/** 选中的是「丢失」、又不与别的候选重叠的分区：可以写回分区表（ext 分区只浏览、另存，不写回） */
const canWriteBack = computed(() => {
  const candidate = selectedCandidate.value
  return (
    candidate !== null &&
    candidate.relation === 'lost' &&
    !candidate.overlapsCandidate &&
    candidate.foundBy !== 'structure' &&
    candidate.filesystem !== 'ext'
  )
})

/** 结果页上正在确认写回分区表或回滚 */
const isConfirming = computed(() => step.value === 'results' && writePlan.plan.value !== null)

/** 刚执行的那次写入（同一个源时才显示）；写下去过、还没回滚的可以回滚 */
const lastRecord = writePlan.lastRecordFor(() => search.target.value)
const canRollbackLast = computed(() => {
  const record = lastRecord.value
  return record !== null && record.kind !== 'rollback' && record.outcome.written > 0
})

const sourceText = computed(() => sourceName(search.target.value))

const statusText = computed(() => {
  switch (search.status.value) {
    case 'starting':
      return isDeviceKind(search.target.value?.kind)
        ? t('common.startingDisk')
        : t('findPartitions.status.starting')
    case 'running':
    case 'cancelling':
      if (search.isPaused.value) {
        return t('findPartitions.status.paused', search.progressParams.value)
      }
      return t('findPartitions.status.running', search.progressParams.value)
    case 'completed':
      return t('findPartitions.status.completed')
    case 'failed':
      return t('findPartitions.status.failed')
    case 'cancelled':
      return t('findPartitions.status.cancelled')
  }
  return ''
})

/** 选中的分区不能写回的原因：按结构找到的长度推不准，ext 分区暂不写回，分区表里已有的不需要写回 */
const candidateNote = computed(() => {
  const candidate = selectedCandidate.value
  if (candidate?.foundBy === 'structure') {
    return t('findPartitions.write.structureOnly')
  }
  if (candidate?.filesystem === 'ext' && candidate.relation === 'lost') {
    return t('findPartitions.write.extUnsupported')
  }
  if (candidate?.relation === 'inTable') {
    return t('findPartitions.write.inTable', { number: candidate.tablePartition ?? '' })
  }
  return null
})

/** 快速搜索的结果页（不在确认写回时）：顶部给出「深度搜索」 */
const canSearchDeeper = computed(
  () =>
    step.value === 'results' &&
    !isConfirming.value &&
    !search.isBusy.value &&
    search.summary.value?.mode === 'quick',
)

const summaryText = computed(() => {
  const summary = search.summary.value
  if (summary === null) {
    return ''
  }
  const parts = [
    t(`findPartitions.results.mode.${summary.mode}`),
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
  if (summary.stoppedAt !== null) {
    parts.push(
      t('findPartitions.results.stopped', {
        position: formatBytes(summary.stoppedAt),
        percent: summary.size > 0 ? Math.floor((summary.stoppedAt / summary.size) * 100) : 0,
      }),
    )
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
  await writePlan.discard()
  await closeBrowse()
  emit('exit')
}

async function goBack() {
  const previous = STEPS[stepIndex.value - 1]
  if (previous === undefined || search.isBusy.value || isPartitionBusy.value) {
    return
  }
  // 结果页回到来源：放弃没确认的计划、关闭浏览，换了源要重新搜索
  await writePlan.discard()
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
  await search.start(current, searchMode.value)
}

/** 结果页上：快速搜索没找到要找的，换深度搜索重新搜一遍 */
async function searchDeep() {
  searchMode.value = 'deep'
  await handleSubmit()
}

/** 搜索完成：进入结果页，有找到分区时打开浏览会话（物理磁盘这时可能再请求一次提权） */
async function enterResults() {
  step.value = 'results'
  const source = search.target.value
  if (source !== null && candidates.value.length > 0) {
    await browse.open(source)
  }
}

/** 把选中的丢失分区写回分区表：先关掉浏览（浏览会话开着这个源），生成计划后显示确认内容 */
async function proposeWriteBack() {
  const candidate = selectedCandidate.value
  const source = search.target.value
  if (candidate === null || source === null || !canWriteBack.value || isPartitionBusy.value) {
    return
  }
  await closeBrowse()
  await writePlan.prepareTableFix(source, {
    kind: 'addLost',
    offset: candidate.offset,
    length: candidate.length,
  })
  if (writePlan.plan.value === null) {
    await reopenBrowse()
  }
}

async function proposeRollback() {
  const record = lastRecord.value
  const source = search.target.value
  if (record === null || source === null || isPartitionBusy.value) {
    return
  }
  await closeBrowse()
  await writePlan.prepareRollback(record.planId, source)
  if (writePlan.plan.value === null) {
    await reopenBrowse()
  }
}

async function confirmWrite() {
  await writePlan.execute()
  if (writePlan.plan.value === null) {
    await reopenBrowse()
  }
}

async function cancelWrite() {
  await writePlan.discard()
  await reopenBrowse()
}

/** 写入计划结束后重新打开浏览，继续浏览、另存 */
async function reopenBrowse() {
  const source = search.target.value
  if (source !== null && candidates.value.length > 0 && !browse.isOpen.value) {
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
    const result = await browse.scan(partition)
    if (serial !== openSerial || result === null) {
      return
    }
  }
  openedPartition.value = partition
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
          <!-- 快速搜索的结果页：没找到要的分区时从这里改用深度搜索 -->
          <button
            v-if="canSearchDeeper"
            class="wizard__button wizard__button--secondary"
            type="button"
            :title="t('findPartitions.results.deepHint')"
            :aria-describedby="`${domPrefix}-deep-hint`"
            :disabled="target === null || isPartitionBusy"
            @click="searchDeep"
          >
            {{ t('findPartitions.results.deep') }}
          </button>
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
            keeps-result
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
          v-model:device="device"
          :disk-hint="t('findPartitions.diskHint')"
        />
        <fieldset class="wizard__choices">
          <legend class="wizard__note">{{ t('findPartitions.mode.legend') }}</legend>
          <label class="wizard__choice">
            <input v-model="searchMode" type="radio" :name="`${domPrefix}-mode`" value="quick" />
            <span>
              {{ t('findPartitions.mode.quick') }}
              <span class="wizard__note">{{ t('findPartitions.mode.quickHint') }}</span>
            </span>
          </label>
          <label class="wizard__choice">
            <input v-model="searchMode" type="radio" :name="`${domPrefix}-mode`" value="deep" />
            <span>
              {{ t('findPartitions.mode.deep') }}
              <span class="wizard__note">{{ t('findPartitions.mode.deepHint') }}</span>
            </span>
          </label>
        </fieldset>
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
            :source-is-disk="isDeviceKind(search.target.value?.kind)"
            context="scan"
          />
          <p v-if="search.isUnresponsive.value" class="wizard__error" role="alert">
            {{ t('common.unresponsive') }}
          </p>
          <p v-if="search.failure.value" class="wizard__error" role="alert">
            {{ formatHostError(search.failure.value) }}
          </p>
        </div>
        <section class="find-partitions__found" :aria-labelledby="`${domPrefix}-found`">
          <h3 :id="`${domPrefix}-found`" class="wizard__section-title find-partitions__title">
            {{ t('findPartitions.live.title', { count: search.found.value.length }) }}
          </h3>
          <ul v-if="search.found.value.length > 0" class="find-partitions__found-list">
            <li
              v-for="candidate in search.found.value"
              :key="`${candidate.offset}-${candidate.length}-${candidate.filesystem}`"
              class="find-partitions__found-item"
            >
              <FoundPartitionItem :candidate="candidate" />
            </li>
          </ul>
          <p v-else class="wizard__note">{{ t('findPartitions.live.none') }}</p>
          <p class="wizard__hint">{{ t('findPartitions.live.hint') }}</p>
        </section>
      </div>

      <div v-else-if="isConfirming && writePlan.plan.value" class="wizard__panel">
        <WritePlanConfirm
          :plan="writePlan.plan.value"
          :sector-size="search.summary.value?.sectorSize ?? 512"
          :busy="writePlan.isExecuting.value"
          :failure="writePlan.failure.value"
          @confirm="confirmWrite"
          @cancel="cancelWrite"
        />
      </div>

      <div v-else class="wizard__panel wizard__fill">
        <dl class="wizard__details wizard__details--inline">
          <div>
            <dt>{{ t('common.source') }}</dt>
            <dd class="wizard__path-text">{{ sourceText }}</dd>
          </div>
          <div>
            <dt>{{ t('findPartitions.results.label') }}</dt>
            <dd>{{ summaryText }}</dd>
          </div>
        </dl>
        <div v-if="lastRecord" class="find-partitions__outcome">
          <p :class="lastRecord.outcome.done ? 'wizard__note' : 'wizard__error'" role="status">
            {{
              t('findPartitions.write.outcome', {
                action: writeActionText(lastRecord),
                outcome: writeOutcomeText(lastRecord),
              })
            }}
          </p>
          <button
            v-if="canRollbackLast"
            class="wizard__button wizard__button--secondary"
            type="button"
            :disabled="isPartitionBusy"
            @click="proposeRollback"
          >
            {{ t('writePlan.history.rollback') }}
          </button>
        </div>
        <p v-if="lastRecord?.outcome.done" class="wizard__hint">{{ t('findPartitions.write.reopenHint') }}</p>
        <p v-if="writePlan.failure.value" class="wizard__error" role="alert">
          {{ formatHostError(writePlan.failure.value) }}
        </p>
        <p v-if="browse.status.value === 'starting'" class="wizard__note" aria-live="polite">
          {{
            isDeviceKind(search.target.value?.kind)
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
          <section class="wizard__pane" :aria-labelledby="`${domPrefix}-list`">
            <h3 :id="`${domPrefix}-list`" class="wizard__section-title find-partitions__title">
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
                  <FoundPartitionItem :candidate="candidate" />
                </label>
                <!-- 选中的分区为什么没有写回：放在它下面、列表框里，不占三栏共用的底部那一行 -->
                <p
                  v-if="index === selectedIndex && candidateNote"
                  class="wizard__hint find-partitions__item-note"
                >
                  {{ candidateNote }}
                </p>
              </li>
            </ul>
            <!-- 这一栏按三行排（标题、列表、下面这些），下面的提示与写回放在同一格里 -->
            <div class="find-partitions__footer">
              <p class="wizard__hint">{{ t('findPartitions.list.hint') }}</p>
              <div v-if="canWriteBack" class="find-partitions__write">
                <template v-if="canWrite">
                  <button
                    class="wizard__button wizard__button--secondary"
                    type="button"
                    :disabled="isPartitionBusy"
                    @click="proposeWriteBack"
                  >
                    {{ t('findPartitions.write.start') }}
                  </button>
                  <p v-if="writePlan.isPreparing.value" class="wizard__note" aria-live="polite">
                    {{ t('findPartitions.write.preparing') }}
                  </p>
                  <p v-else class="wizard__hint">{{ t('findPartitions.write.hint') }}</p>
                </template>
                <p v-else class="wizard__hint">{{ t('findPartitions.write.volumeUnsupported') }}</p>
              </div>
            </div>
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
            class="wizard__pane find-partitions__pane--wide"
            :aria-labelledby="`${domPrefix}-files`"
          >
            <h3 :id="`${domPrefix}-files`" class="wizard__section-title find-partitions__title">
              {{ t('findPartitions.files.title') }}
            </h3>
            <div class="find-partitions__box" aria-live="polite">
              <p v-if="selectedCandidate === null" class="wizard__note">
                {{ t('findPartitions.files.pick') }}
              </p>
              <p v-else-if="isOpeningPartition" class="wizard__note">
                {{ t('findPartitions.files.opening') }}
              </p>
              <DeletedScanProgress v-else-if="browse.isScanning.value" :browse="browse" />
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
        <!-- 固定的说明放在最下面，顶部只留来源、结果与写盘的结论 -->
        <!-- 深度搜索的说明悬停按钮可看；这一份给读屏软件读 -->
        <p v-if="canSearchDeeper" :id="`${domPrefix}-deep-hint`" class="wizard__visually-hidden">
          {{ t('findPartitions.results.deepHint') }}
        </p>
        <p class="wizard__hint wizard__hint--warning">{{ t('findPartitions.results.readOnly') }}</p>
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

.find-partitions__pane--wide {
  grid-column: 2 / -1;
}

.find-partitions__title {
  align-self: center;
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

.find-partitions__item-note {
  margin: 0;
  padding: 0 var(--space-sm) var(--space-sm);
  background: color-mix(in srgb, var(--color-accent) 16%, transparent);
  border-bottom: 1px solid var(--color-border);
}

.find-partitions__item--selected {
  background: color-mix(in srgb, var(--color-accent) 16%, transparent);
}

/* 搜索进行中已经找到的分区：按内容撑高，太多时在框里滚动 */
.find-partitions__found {
  display: flex;
  flex-direction: column;
  gap: var(--space-sm);
  width: 100%;
}

.find-partitions__found-list {
  max-height: 40vh;
  margin: 0;
  padding: 0;
  overflow-y: auto;
  list-style: none;
  border: 1px solid var(--color-border);
  border-radius: var(--radius-card);
}

.find-partitions__found-item {
  display: flex;
  padding: var(--space-sm);
  border-bottom: 1px solid var(--color-border);
}

.find-partitions__footer {
  display: flex;
  flex-direction: column;
  gap: var(--space-sm);
}

.find-partitions__outcome,
.find-partitions__write {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-sm);
}
</style>
