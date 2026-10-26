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

const emit = defineEmits<{ exit: [] }>()

const { t, te } = useI18n()
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
const outputDir = ref('')
const dialogError = ref<HostError | null>(null)
const fileActionError = ref<HostError | null>(null)
const fileActionNote = ref<string | null>(null)
/** 当前的复制任务是为「打开」而复制的单个文件，复制完就打开它 */
const isCopyingToOpen = ref(false)

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
const trimmedOutputDir = computed(() => outputDir.value.trim())
const isSourceDisk = computed(() => browse.target.value?.kind === 'disk')

const canGoNext = computed(() => {
  switch (step.value) {
    case 'source':
      return target.value !== null && !browse.isTransitioning.value
    case 'partition':
      return currentPartition.value?.browsable === true
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
  switch (copy.status.value) {
    case 'starting':
      return t('undelete.copy.status.starting')
    case 'running':
    case 'cancelling':
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

// 为「打开」复制的文件复制好了就打开；可执行与脚本类文件宿主不打开，改为在文件夹中显示
watch(copy.status, async (status) => {
  if (!isCopyingToOpen.value || !['completed', 'failed', 'cancelled'].includes(status)) {
    return
  }
  isCopyingToOpen.value = false
  const jobId = copy.jobId.value
  const first = copy.succeeded.value[0]
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
  if (previous === undefined || copy.isBusy.value) {
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
    step.value = 'browse'
  }
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

async function browseOutputDir() {
  const picked = await pickOutputDir()
  if (picked !== null) {
    outputDir.value = picked
  }
}

/** 系统的选择文件夹对话框；用户取消时返回 null */
async function pickOutputDir(): Promise<string | null> {
  dialogError.value = null
  try {
    return await pickDirectory(t('undelete.copy.pickTitle'))
  } catch (error) {
    dialogError.value = toHostError(error)
    return null
  }
}

/** 还没选保存位置时先弹出选择文件夹对话框 */
async function ensureOutputDir(): Promise<string | null> {
  if (trimmedOutputDir.value !== '') {
    return trimmedOutputDir.value
  }
  const picked = await pickOutputDir()
  if (picked !== null) {
    outputDir.value = picked
  }
  return picked
}

async function handleCopy() {
  if (!canCopy.value) {
    return
  }
  const dir = await ensureOutputDir()
  if (dir === null) {
    return
  }
  await startCopy(copyItems.value, dir)
}

/** 双击文件或点「用默认程序打开」：先把它复制到保存位置，复制好再交给系统打开 */
async function openItem(item: TreeItem) {
  if (item.entry.isDir || copy.isBusy.value) {
    return
  }
  selectedItem.value = item
  fileActionError.value = null
  fileActionNote.value = null
  const dir = await ensureOutputDir()
  if (dir === null) {
    return
  }
  isCopyingToOpen.value = true
  await startCopy([{ file: item.entry.file, path: [item.entry.name] }], dir)
  if (copy.status.value === 'failed' && isCopyingToOpen.value) {
    isCopyingToOpen.value = false
    fileActionError.value = copy.failure.value
  }
}

async function startCopy(items: CopyItem[], dir: string) {
  const source = browse.target.value
  const partition = currentPartition.value
  if (source === null || partition === null) {
    return
  }
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
        {{ t('carve.exit') }}
      </button>
      <h2 id="undelete-wizard-title" ref="titleElement" class="wizard__title" tabindex="-1">
        {{ t('scenarios.undelete') }}
      </h2>
    </header>

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

    <form class="wizard__form" @submit.prevent="handleSubmit">
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
                  :disabled="!partition.browsable"
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
        <p v-if="isSourceDisk" class="wizard__hint">{{ t('undelete.browse.liveHint') }}</p>
        <p v-if="browse.isUnresponsive.value" class="wizard__error" role="alert">
          {{ t('carve.run.unresponsive') }}
        </p>
        <p v-if="!browse.isOpen.value && browse.failure.value" class="wizard__error" role="alert">
          {{ formatHostError(browse.failure.value) }}
        </p>
        <label class="undelete-wizard__filter">
          <input v-model="deletedOnly" type="checkbox" />
          {{ t('undelete.browse.deletedOnly') }}
        </label>

        <div class="undelete-wizard__files">
          <FsBrowser
            v-model:selected="selectedItem"
            v-model:checked="checkedItems"
            :job-id="browse.jobId.value"
            :partition="currentPartition.number"
            :deleted-only="deletedOnly"
            @open="openItem"
          />
          <FsEntryDetails
            :job-id="browse.jobId.value"
            :partition="currentPartition.number"
            :item="selectedItem"
            :sector-size="browse.opened.value?.sectorSize ?? 512"
            :can-open="!copy.isBusy.value && browse.isOpen.value"
            :action-error="fileActionError"
            :action-note="fileActionNote"
            @open="openItem"
          />
        </div>
        <p class="wizard__hint">{{ t('undelete.browse.stateHint') }}</p>

        <div class="wizard__path">
          <label class="wizard__label" for="undelete-output-dir">{{ t('undelete.copy.label') }}</label>
          <input
            id="undelete-output-dir"
            v-model="outputDir"
            class="wizard__input"
            type="text"
            spellcheck="false"
            autocomplete="off"
            :placeholder="t('undelete.copy.placeholder')"
          />
          <button
            class="wizard__button wizard__button--secondary"
            type="button"
            @click="browseOutputDir"
          >
            {{ t('carve.browseFolder') }}
          </button>
        </div>
        <p class="wizard__hint">{{ t('undelete.copy.hint') }}</p>
        <p v-if="isSourceDisk" class="wizard__hint wizard__hint--warning">
          {{ t('carve.output.diskHint') }}
        </p>
        <div class="undelete-wizard__copy-actions">
          <button class="wizard__button" type="button" :disabled="!canCopy" @click="handleCopy">
            {{ t('undelete.copy.start', { count: copyItems.length }) }}
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

        <div v-if="copy.status.value !== 'idle'" class="wizard__job" aria-live="polite">
          <progress
            class="wizard__progress"
            :value="copy.done.value"
            :max="copy.total.value || 1"
          ></progress>
          <p class="wizard__status">{{ copyStatusText }}</p>
          <p v-if="copy.failure.value" class="wizard__error" role="alert">
            {{ formatHostError(copy.failure.value) }}
          </p>
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

      <div class="wizard__nav">
        <button
          v-if="stepIndex > 0"
          class="wizard__button wizard__button--secondary"
          type="button"
          :disabled="copy.isBusy.value"
          @click="goBack"
        >
          {{ t('carve.previous') }}
        </button>
        <span class="wizard__nav-gap"></span>
        <button v-if="step !== 'browse'" class="wizard__button" type="submit" :disabled="!canGoNext">
          {{ t('carve.next') }}
        </button>
      </div>
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

.undelete-wizard__files {
  display: grid;
  grid-template-columns: minmax(0, 3fr) minmax(0, 2fr);
  gap: var(--space-lg);
  align-items: start;
}

.undelete-wizard__copy-actions {
  display: flex;
  gap: var(--space-sm);
}
</style>
