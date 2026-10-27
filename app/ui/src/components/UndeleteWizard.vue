<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import FsBrowser, { type TreeItem } from './FsBrowser.vue'
import FsEntryDetails from './FsEntryDetails.vue'
import SourcePicker from './SourcePicker.vue'
import { useBrowse } from '../composables/useBrowse'
import { useCopy } from '../composables/useCopy'
import { useHostErrorText } from '../composables/useHostErrorText'
import {
  openCopiedFile,
  pickDirectory,
  revealCopiedFile,
  startCopy as startCopyJob,
  toHostError,
  type CopyItem,
  type HostError,
  type JobTarget,
  type PartitionInfo,
} from '../services/host'
import { formatBytes } from '../utils/format'

const STEPS = ['source', 'partition', 'browse'] as const
type WizardStep = (typeof STEPS)[number]
/** 复制失败的文件最多列出这么多个 */
const MAX_LISTED_FAILURES = 20
/** 为打开而复制时，宿主拒绝写程序目录下的 open-cache（例如程序就在源盘上），改请用户选保存位置 */
const OPEN_COPY_REFUSED = ['outputOnSource', 'outputUnavailable']
const FINISHED_STATUSES: string[] = ['completed', 'failed', 'cancelled']

const emit = defineEmits<{ exit: [] }>()

const { t, te, locale } = useI18n()
const { formatHostError } = useHostErrorText()
const browse = useBrowse()
const copy = useCopy()

const titleElement = ref<HTMLHeadingElement | null>(null)
const step = ref<WizardStep>('source')
const sourceKind = ref<JobTarget['kind']>('image')
const imagePath = ref('')
const selectedDisk = ref<number | null>(null)
const selectedPartition = ref<number | null>(null)
const deletedOnly = ref(false)
const selectedItem = ref<TreeItem | null>(null)
const checkedItems = ref(new Map<string, TreeItem>())
const dialogError = ref<HostError | null>(null)
const fileActionError = ref<HostError | null>(null)
const fileActionNote = ref<string | null>(null)
/** 当前的复制任务是另存为，还是为「打开」而复制单个文件 */
const copyPurpose = ref<'saveAs' | 'open'>('saveAs')
/** 为打开而复制的文件，复制好就打开它 */
const openingItem = ref<TreeItem | null>(null)
/** 当前的复制任务写的是程序目录下的 open-cache */
const isOpenCacheCopy = ref(false)

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

/** 勾选的项里，上级目录也被勾选的不再单列：目录本来就连同内容一起复制 */
const copyItems = computed<CopyItem[]>(() => {
  const ids = [...checkedItems.value.keys()]
  return [...checkedItems.value.values()]
    .filter((item) => !ids.some((other) => other !== item.id && item.id.startsWith(`${other}/`)))
    .map((item) => ({ file: item.entry.file, path: item.path }))
})

/** 当前分区扫描已删除项的结果（只有 NTFS 有） */
const currentScan = computed(() =>
  currentPartition.value === null
    ? null
    : (browse.scanResults.value.get(currentPartition.value.number) ?? null),
)
const needsScanHint = computed(() => partitions.value.some((partition) => partition.needsScan))

const scanStatusText = computed(() =>
  browse.scanTotal.value > 0
    ? t('undelete.scan.running', {
        done: formatBytes(browse.scanDone.value),
        total: formatBytes(browse.scanTotal.value),
        percent: browse.scanPercent.value,
      })
    : t('undelete.scan.starting'),
)

/** 复制出来全是 0 的文件 */
const zeroFiles = computed(() => copy.succeeded.value.filter((file) => file.allZero))

const canCopy = computed(
  () => copyItems.value.length > 0 && !copy.isBusy.value && browse.isOpen.value,
)

const sourceText = computed(() => {
  const current = browse.target.value
  if (current === null) {
    return ''
  }
  return current.kind === 'image'
    ? t('undelete.sourceImage', { path: current.path })
    : t('undelete.sourceDisk', { number: current.diskNumber })
})

const openingText = computed(() =>
  target.value?.kind === 'disk' ? t('undelete.openingDisk') : t('undelete.opening'),
)

const copyStatusText = computed(() => {
  if (copyPurpose.value === 'open') {
    return t('undelete.copy.preparing')
  }
  switch (copy.status.value) {
    case 'starting':
      return t('undelete.copy.status.starting')
    case 'running':
    case 'cancelling':
      if (copy.stage.value === 'scanMft') {
        return t('undelete.copy.status.scanning', { percent: copy.percent.value })
      }
      return t('undelete.copy.status.running', {
        done: formatBytes(copy.done.value),
        total: formatBytes(copy.total.value),
        percent: copy.percent.value,
        count: copy.succeeded.value.length,
      })
    case 'completed':
      return t('undelete.copy.status.completed')
    case 'failed':
      return t('undelete.copy.status.failed')
    case 'cancelled':
      return t('undelete.copy.status.cancelled')
  }
  return ''
})

watch(selectedPartition, () => {
  selectedItem.value = null
  checkedItems.value = new Map()
})

watch(selectedItem, () => {
  fileActionError.value = null
  fileActionNote.value = null
})

/** 另存为的进度与结果一直显示；为打开而复制只在进行中显示一行状态 */
const isCopyStatusShown = computed(
  () => copy.status.value !== 'idle' && (copyPurpose.value === 'saveAs' || copy.isBusy.value),
)

watch(copy.status, (status) => {
  const item = openingItem.value
  if (item !== null && FINISHED_STATUSES.includes(status)) {
    void finishOpen(item)
  }
})

onMounted(() => {
  titleElement.value?.focus()
})

async function handleExit() {
  if (copy.isBusy.value) {
    return
  }
  if (browse.isOpen.value) {
    await browse.close()
  }
  emit('exit')
}

async function goBack() {
  const previous = STEPS[stepIndex.value - 1]
  if (previous === undefined || copy.isBusy.value || browse.isScanning.value) {
    return
  }
  dialogError.value = null
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

/**
 * 为「打开」复制的文件复制好了就打开；可执行与脚本类文件宿主不打开，改为在文件夹中显示。
 * watch 与 copyToOpen 都可能先调到这里，只处理一次
 */
async function finishOpen(item: TreeItem) {
  if (openingItem.value !== item) {
    return
  }
  openingItem.value = null
  const status = copy.status.value
  const jobId = copy.jobId.value
  const first = copy.succeeded.value[0]
  const failureCode = copy.failure.value?.code ?? ''
  if (status === 'failed' && isOpenCacheCopy.value && OPEN_COPY_REFUSED.includes(failureCode)) {
    const dir = await pickOutputDir(t('undelete.copy.fallbackTitle'))
    if (dir !== null) {
      await copyToOpen(item, dir)
    }
    return
  }
  if (status !== 'completed' || jobId === null || first === undefined) {
    fileActionError.value = copy.failure.value ?? {
      code: 'jobFailed',
      detail: copy.failures.value[0]?.error ?? '',
    }
    return
  }
  try {
    await openCopiedFile(jobId, first.path)
  } catch (error) {
    const hostError = toHostError(error)
    if (hostError.code !== 'executableBlocked') {
      fileActionError.value = hostError
      return
    }
    try {
      await revealCopiedFile(jobId, first.path)
      fileActionNote.value = t('undelete.details.executableRevealed')
    } catch (revealError) {
      fileActionError.value = toHostError(revealError)
    }
  }
}

/** 系统的选择文件夹对话框；用户取消时返回 null */
async function pickOutputDir(title: string): Promise<string | null> {
  dialogError.value = null
  try {
    return await pickDirectory(title)
  } catch (error) {
    dialogError.value = toHostError(error)
    return null
  }
}

/** 另存为：每次都选保存位置，选好就开始复制 */
async function handleSaveAs() {
  if (!canCopy.value) {
    return
  }
  const dir = await pickOutputDir(t('undelete.copy.pickTitle'))
  if (dir === null) {
    return
  }
  await startCopy(copyItems.value, dir, 'saveAs')
}

/** 双击文件或点「用默认程序打开」：先在 open-cache 里复制一份，复制好再交给系统打开 */
async function openItem(item: TreeItem) {
  if (item.entry.isDir || item.entry.unsupported !== null || copy.isBusy.value) {
    return
  }
  selectedItem.value = item
  fileActionError.value = null
  fileActionNote.value = null
  await copyToOpen(item, null)
}

/** `dir` 为 null 时复制到程序目录下的 open-cache；用户另选了位置时按另存为显示结果，让人知道存在了哪 */
async function copyToOpen(item: TreeItem, dir: string | null) {
  openingItem.value = item
  await startCopy(
    [{ file: item.entry.file, path: [item.entry.name] }],
    dir,
    dir === null ? 'open' : 'saveAs',
  )
  // 启动就失败、而上一个任务也是失败时，状态没有变化，watch 不触发，这里补上
  if (FINISHED_STATUSES.includes(copy.status.value)) {
    await finishOpen(item)
  }
}

async function startCopy(items: CopyItem[], dir: string | null, purpose: 'saveAs' | 'open') {
  const source = browse.target.value
  const partition = currentPartition.value
  if (source === null || partition === null) {
    return
  }
  copyPurpose.value = purpose
  isOpenCacheCopy.value = dir === null
  const partitionRef = { number: partition.number, offset: partition.offset, length: partition.length }
  await copy.start((jobId) => startCopyJob(jobId, source, partitionRef, items, dir))
}

async function revealOutput() {
  const jobId = copy.jobId.value
  const first = copy.succeeded.value[0]
  if (jobId === null || first === undefined) {
    return
  }
  dialogError.value = null
  try {
    await revealCopiedFile(jobId, first.path)
  } catch (error) {
    dialogError.value = toHostError(error)
  }
}

function partitionRole(partition: PartitionInfo): string {
  const key = `partition.role.${partition.role ?? ''}`
  if (partition.role !== null && te(key)) {
    return t(key)
  }
  return partition.typeId === '' ? '-' : partition.typeId
}

function partitionFs(partition: PartitionInfo): string {
  const key = `partition.fs.${partition.filesystem}`
  return te(key) ? t(key) : partition.filesystem
}

/** 不能浏览的原因；文件系统认得出、却打不开时按「参数不对」说 */
function unsupportedReason(partition: PartitionInfo): string {
  const key = `undelete.partition.reason.${partition.filesystem}`
  return te(key) ? t(key) : t('undelete.partition.reason.invalid')
}

function noteText(note: string): string {
  const key = `undelete.notes.${note}`
  return te(key) ? t(key) : note
}
</script>

<template>
  <section class="wizard" aria-labelledby="undelete-wizard-title">
    <header class="wizard__header">
      <button
        class="wizard__button wizard__button--secondary"
        type="button"
        :disabled="copy.isBusy.value"
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
            :disabled="copy.isBusy.value || browse.isScanning.value"
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
          <li v-for="note in browse.opened.value.notes" :key="note">{{ noteText(note) }}</li>
        </ul>
        <table class="wizard__table undelete-wizard__partitions">
          <caption class="wizard__caption">{{ t('undelete.partition.caption') }}</caption>
          <thead>
            <tr>
              <th scope="col"><span class="undelete-wizard__visually-hidden">{{ t('undelete.partition.choose') }}</span></th>
              <th scope="col">{{ t('undelete.partition.number') }}</th>
              <th scope="col">{{ t('undelete.partition.label') }}</th>
              <th scope="col">{{ t('undelete.partition.filesystem') }}</th>
              <th scope="col">{{ t('undelete.partition.role') }}</th>
              <th scope="col" class="wizard__number">{{ t('undelete.partition.start') }}</th>
              <th scope="col" class="wizard__number">{{ t('undelete.partition.size') }}</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="partition in partitions"
              :key="partition.number"
              :class="{ 'undelete-wizard__row--disabled': !partition.browsable }"
            >
              <td>
                <input
                  :id="`undelete-partition-${partition.number}`"
                  v-model="selectedPartition"
                  type="radio"
                  name="undelete-partition"
                  :value="partition.number"
                  :disabled="!partition.browsable || browse.isScanning.value"
                />
              </td>
              <th scope="row">
                <label :for="`undelete-partition-${partition.number}`">
                  {{ partition.number }}{{ partition.logical ? ` ${t('undelete.partition.logical')}` : '' }}
                </label>
              </th>
              <td>{{ partition.label ?? partition.name ?? '' }}</td>
              <td>
                {{ partitionFs(partition) }}
                <span v-if="!partition.browsable" class="wizard__note">
                  · {{ unsupportedReason(partition) }}
                </span>
              </td>
              <td>{{ partitionRole(partition) }}</td>
              <td class="wizard__number">{{ formatBytes(partition.offset) }}</td>
              <td class="wizard__number">{{ formatBytes(partition.length) }}</td>
            </tr>
          </tbody>
        </table>
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

      <div v-else-if="currentPartition && browse.jobId.value" class="wizard__panel">
        <dl class="wizard__details">
          <dt>{{ t('undelete.browse.source') }}</dt>
          <dd class="wizard__path-text">{{ sourceText }}</dd>
          <dt>{{ t('undelete.browse.partition') }}</dt>
          <dd>
            {{
              t('undelete.browse.partitionValue', {
                number: currentPartition.number,
                label: currentPartition.label ?? '',
                filesystem: partitionFs(currentPartition),
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
            {{ t('undelete.scan.orphans', { count: currentScan.orphans.toLocaleString(locale) }) }}
          </template>
          <template v-if="currentScan.unreadable > 0">
            {{ t('undelete.scan.unreadable', { count: currentScan.unreadable.toLocaleString(locale) }) }}
          </template>
        </p>
        <p v-if="isSourceDisk" class="wizard__hint">{{ t('undelete.browse.liveHint') }}</p>
        <p v-if="browse.isUnresponsive.value" class="wizard__error" role="alert">
          {{ t('carve.run.unresponsive') }}
        </p>
        <p v-if="!browse.isOpen.value && browse.failure.value" class="wizard__error" role="alert">
          {{ formatHostError(browse.failure.value) }}
        </p>
        <div class="undelete-wizard__files">
          <FsBrowser
            v-model:selected="selectedItem"
            v-model:checked="checkedItems"
            class="undelete-wizard__pane"
            :job-id="browse.jobId.value"
            :partition="currentPartition.number"
            :deleted-only="deletedOnly"
            @open="openItem"
          >
            <template #tools>
              <label class="undelete-wizard__filter">
                <input v-model="deletedOnly" type="checkbox" />
                {{ t('undelete.browse.deletedOnly') }}
              </label>
            </template>
            <template #actions>
              <div class="undelete-wizard__save-as">
                <button class="wizard__button" type="button" :disabled="!canCopy" @click="handleSaveAs">
                  {{ t('undelete.copy.saveAs', { count: copyItems.length }) }}
                </button>
                <button
                  v-if="copy.isBusy.value"
                  class="wizard__button wizard__button--secondary"
                  type="button"
                  :disabled="copy.status.value !== 'running'"
                  @click="copy.cancel"
                >
                  {{ copy.status.value === 'cancelling' ? t('carve.run.cancelling') : t('carve.run.cancel') }}
                </button>
              </div>
            </template>
          </FsBrowser>
          <FsEntryDetails
            class="undelete-wizard__pane"
            :job-id="browse.jobId.value"
            :partition="currentPartition.number"
            :item="selectedItem"
            :sector-size="browse.opened.value?.sectorSize ?? 512"
            :times-utc="currentPartition.timesUtc"
            :can-open="!copy.isBusy.value && browse.isOpen.value"
            :action-error="fileActionError"
            :action-note="fileActionNote"
            @open="openItem"
          >
            <template #footer>
              <div class="undelete-wizard__pane-notes">
                <p class="wizard__hint">{{ t('undelete.browse.stateHint') }}</p>
                <p v-if="currentPartition.timesUtc" class="wizard__hint">
                  {{ t('undelete.browse.timesUtc') }}
                </p>
              </div>
            </template>
          </FsEntryDetails>
        </div>

        <div v-if="isCopyStatusShown" class="wizard__job" aria-live="polite">
          <progress
            v-if="copyPurpose === 'saveAs'"
            class="wizard__progress"
            :value="copy.done.value"
            :max="copy.total.value || 1"
          ></progress>
          <p class="wizard__status">{{ copyStatusText }}</p>
          <p v-if="copy.failure.value" class="wizard__error" role="alert">
            {{ formatHostError(copy.failure.value) }}
          </p>
          <template v-if="zeroFiles.length > 0">
            <p class="wizard__hint wizard__hint--warning">
              {{ t('undelete.copy.allZero', { count: zeroFiles.length }) }}
            </p>
            <ul class="undelete-wizard__failures">
              <li v-for="file in zeroFiles.slice(0, MAX_LISTED_FAILURES)" :key="file.path">
                {{ file.path }}
              </li>
            </ul>
          </template>
          <template v-if="copy.failures.value.length > 0">
            <p class="wizard__error">
              {{ t('undelete.copy.failedCount', { count: copy.failures.value.length }) }}
            </p>
            <ul class="undelete-wizard__failures">
              <li
                v-for="failed in copy.failures.value.slice(0, MAX_LISTED_FAILURES)"
                :key="failed.path"
              >
                {{ t('undelete.copy.failedItem', { path: failed.path, reason: failed.error }) }}
              </li>
            </ul>
          </template>
          <dl v-if="copy.summary.value" class="wizard__details">
            <dt>{{ t('undelete.copy.report.files') }}</dt>
            <dd>
              {{
                t('undelete.copy.report.filesValue', {
                  count: copy.summary.value.files,
                  bytes: formatBytes(copy.summary.value.bytes),
                })
              }}
            </dd>
            <dt>{{ t('undelete.copy.report.elapsed') }}</dt>
            <dd>
              {{
                t('carve.run.report.elapsedValue', {
                  seconds: (copy.summary.value.elapsedMs / 1000).toFixed(1),
                })
              }}
            </dd>
            <dt>{{ t('undelete.copy.report.outputDir') }}</dt>
            <dd class="wizard__path-text">{{ copy.summary.value.outputDir }}</dd>
          </dl>
          <div v-if="copy.summary.value && copy.succeeded.value.length > 0">
            <button
              class="wizard__button wizard__button--secondary"
              type="button"
              @click="revealOutput"
            >
              {{ t('undelete.copy.reveal') }}
            </button>
          </div>
        </div>
      </div>

      <p v-if="dialogError" class="wizard__error" role="alert">
        {{ formatHostError(dialogError) }}
      </p>
    </form>
  </section>
</template>

<style scoped>
.undelete-wizard__notes,
.undelete-wizard__failures {
  margin: 0;
  padding-left: var(--space-lg);
  font-size: var(--font-size-small);
}

.undelete-wizard__failures {
  max-height: 160px;
  overflow-y: auto;
  overflow-wrap: anywhere;
}

.undelete-wizard__partitions {
  max-width: none;
}

.undelete-wizard__row--disabled {
  color: var(--color-text-muted);
}

.undelete-wizard__visually-hidden {
  position: absolute;
  width: 1px;
  height: 1px;
  overflow: hidden;
  clip-path: inset(50%);
  white-space: nowrap;
}

.undelete-wizard__filter {
  display: flex;
  align-items: center;
  gap: var(--space-xs);
  cursor: pointer;
}

/* 列表与详情两栏都分标题、框体、底部三行，三行上下边对齐；列表要放名字、状态与时间，占得宽一些 */
.undelete-wizard__files {
  display: grid;
  grid-template-columns: minmax(0, 3fr) minmax(0, 2fr);
  grid-template-rows: auto auto auto;
  gap: var(--space-sm) var(--space-lg);
}

.undelete-wizard__files > .undelete-wizard__pane {
  display: grid;
  grid-row: span 3;
  grid-template-rows: subgrid;
}

.undelete-wizard__save-as {
  display: flex;
  flex: none;
  gap: var(--space-sm);
}

.undelete-wizard__pane-notes {
  display: flex;
  flex-direction: column;
  gap: var(--space-xs);
}
</style>
