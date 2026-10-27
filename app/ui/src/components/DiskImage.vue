<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import ImageHistoryList from './ImageHistoryList.vue'
import ImageRegionPicker from './ImageRegionPicker.vue'
import SourcePicker from './SourcePicker.vue'
import UnreadableRanges from './UnreadableRanges.vue'
import { useDiskImage } from '../composables/useDiskImage'
import { useHostDialog } from '../composables/useHostDialog'
import { useHostErrorText } from '../composables/useHostErrorText'
import { useImageHistory } from '../composables/useImageHistory'
import { useImageInspection } from '../composables/useImageInspection'
import { usePartitionText } from '../composables/usePartitionText'
import {
  listDisks,
  pickFile,
  toHostError,
  type ByteRange,
  type DiskSummary,
  type HostError,
  type ImageHistoryItem,
  type ImagePlan,
  type ImageRegion,
  type JobTarget,
} from '../services/host'
import { formatBytes, formatSeconds } from '../utils/format'

/** 报告里最多列出的读不出区段数；其余只计入总数 */
const MAX_LISTED_RANGES = 20
/** 改完路径停下这么久再去看源与镜像位置，不在每敲一个字时都去读 */
const INSPECT_DELAY_MS = 700

const emit = defineEmits<{ exit: [] }>()

const { t } = useI18n()
const { formatHostError } = useHostErrorText()
const { diskName, sourceName, regionLine } = usePartitionText()
const {
  status,
  imaging,
  summary,
  lastRequest,
  failure,
  isUnresponsive,
  isBusy,
  start,
  continueLast,
  cancel,
} = useDiskImage()
const history = useImageHistory()
const {
  inspection,
  inspected,
  isInspecting,
  failure: inspectFailure,
  inspect,
  clear: clearInspection,
} = useImageInspection()
const { dialogError, pickFolder } = useHostDialog()

const titleElement = ref<HTMLHeadingElement | null>(null)
const pathInput = ref<HTMLInputElement | null>(null)
// 做镜像通常是为了出问题的物理盘，默认选它
const sourceKind = ref<JobTarget['kind']>('disk')
const sourcePath = ref('')
const selectedDisk = ref<number | null>(null)
const imagePath = ref('')
/** 整盘，或者只做选中的几段 */
const scopeMode = ref<'whole' | 'select'>('whole')
const selectedRegions = ref<number[]>([])
/** 清掉所选范围已有的进度，从头重读 */
const redoSelected = ref(false)
const copyOnly = ref(false)
/** 源在上次之后变过时怎么办：重做变化的部分，或保留旧内容接着做 */
const changeChoice = ref<'redo' | 'keep' | null>(null)
const disks = ref<DiskSummary[]>([])
/** 从做过的镜像里选了一个，原来的盘却没接上 */
const historyNote = ref('')
const pickError = ref<HostError | null>(null)
let inspectTimer: ReturnType<typeof setTimeout> | null = null

const imageTarget = computed<JobTarget | null>(() => {
  if (sourceKind.value === 'image') {
    const path = sourcePath.value.trim()
    return path === '' ? null : { kind: 'image', path }
  }
  return selectedDisk.value === null ? null : { kind: 'disk', diskNumber: selectedDisk.value }
})
const trimmedImagePath = computed(() => imagePath.value.trim())

/** 手上的查看结果正是当前的源与路径的 */
const current = computed(() => {
  const target = imageTarget.value
  const seen = inspected.value
  if (target === null || seen === null || inspection.value === null) {
    return null
  }
  const sameTarget =
    target.kind === seen.target.kind &&
    (target.kind === 'image'
      ? seen.target.kind === 'image' && target.path === seen.target.path
      : seen.target.kind === 'disk' && target.diskNumber === seen.target.diskNumber)
  return sameTarget && seen.imagePath === trimmedImagePath.value ? inspection.value : null
})
const existing = computed(() => current.value?.existing ?? null)
/** 这个位置已有能续做的镜像 */
const canResume = computed(() => existing.value?.state === 'ok')
/** 这个位置已有镜像或坏区图，却不能续做：要换个位置 */
const isBlocked = computed(() => existing.value !== null && existing.value.state !== 'ok')

/** 能不能只做一部分：镜像要是稀疏文件（已有的镜像看它自己，新建的看输出盘） */
const partialAllowed = computed(() => {
  const inspected = current.value
  if (inspected === null) {
    return false
  }
  return canResume.value ? (existing.value?.sparse ?? false) : (inspected.output?.sparse ?? false)
})
const partialBlockedReason = computed(() => {
  const inspected = current.value
  if (inspected === null || partialAllowed.value) {
    return ''
  }
  if (canResume.value) {
    return t('diskImage.scope.oldImage')
  }
  return inspected.output === null
    ? t('diskImage.scope.outputUnknown')
    : t('diskImage.scope.notSparse', { fileSystem: inspected.output.fileSystem })
})
const isSelecting = computed(() => scopeMode.value === 'select' && partialAllowed.value)

const chosenRegions = computed<ImageRegion[]>(() =>
  (current.value?.regions ?? []).filter((region) => selectedRegions.value.includes(region.offset)),
)

/** 源在做镜像之后变过的部分：分区表里变了的分区（变之前与之后的位置），抽查对不上的段 */
const changedRanges = computed<ByteRange[]>(() => {
  const inspected = current.value
  if (inspected === null || !canResume.value) {
    return []
  }
  const ranges: ByteRange[] = []
  for (const change of existing.value?.partitionChanges ?? []) {
    if (change.before) ranges.push(change.before)
    if (change.after) ranges.push(change.after)
  }
  for (const region of inspected.regions) {
    if ((region.check?.mismatched ?? 0) > 0) {
      ranges.push({ offset: region.offset, length: region.length })
    }
  }
  return ranges
})
const mismatchedRegions = computed(() =>
  (current.value?.regions ?? []).filter((region) => (region.check?.mismatched ?? 0) > 0),
)
const hasChanges = computed(() => changedRanges.value.length > 0)

const plan = computed<ImagePlan>(() => {
  const size = current.value?.size ?? 0
  const ranges = isSelecting.value
    ? chosenRegions.value.map((region) => ({ offset: region.offset, length: region.length }))
    : undefined
  const reset: ByteRange[] = []
  if (redoSelected.value && canResume.value) {
    reset.push(...(ranges ?? [{ offset: 0, length: size }]))
  }
  if (changeChoice.value === 'redo') {
    reset.push(...changedRanges.value)
  }
  return { ranges, reset, copyOnly: copyOnly.value }
})

/** 这一次最多还要写入多少：范围里还没读完的（清掉进度的按整段算），只读一部分时加上分区表区域 */
const bytesToWrite = computed(() => {
  const inspected = current.value
  if (inspected === null) {
    return 0
  }
  const regions = isSelecting.value ? chosenRegions.value : inspected.regions
  const redo = redoSelected.value && canResume.value
  const unfinished = regions.reduce((sum, region) => {
    const { untriedBytes, trimBytes, scrapeBytes } = region.progress
    return sum + (redo ? region.length : untriedBytes + trimBytes + scrapeBytes)
  }, 0)
  if (!isSelecting.value) {
    // 不选范围时整个源都算，包括没单独列出的小空隙
    const whole = existing.value?.progress
    if (whole === undefined || redo) {
      return inspected.size
    }
    return whole.untriedBytes + whole.trimBytes + whole.scrapeBytes
  }
  return unfinished + inspected.tableAreas.reduce((sum, area) => sum + area.length, 0)
})
/** 稀疏镜像不预留空间：输出盘放不下这一次要写的量时不让开始 */
const isShortOfSpace = computed(() => {
  const output = current.value?.output
  return output !== null && output !== undefined && partialAllowed.value && output.freeBytes < bytesToWrite.value
})
/** 选中的都已做完、又没勾重做：这一次没有要读的 */
const nothingToDo = computed(
  () =>
    isSelecting.value &&
    chosenRegions.value.length > 0 &&
    !redoSelected.value &&
    changeChoice.value !== 'redo' &&
    chosenRegions.value.every(
      (region) =>
        region.progress.untriedBytes +
          (copyOnly.value ? 0 : region.progress.trimBytes + region.progress.scrapeBytes) ===
        0,
    ),
)

const canStart = computed(
  () =>
    imageTarget.value !== null &&
    trimmedImagePath.value !== '' &&
    !isBusy.value &&
    current.value !== null &&
    !isInspecting.value &&
    !isBlocked.value &&
    (!isSelecting.value || chosenRegions.value.length > 0) &&
    (!hasChanges.value || changeChoice.value !== null) &&
    !isShortOfSpace.value &&
    !nothingToDo.value,
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

/** 已经有结论的字节数（读出或确认读不出），进度条按它走；只做复制这一轮时读错的块也算过了这一遍 */
const settledBytes = computed(() => {
  const now = imaging.value
  if (now === null) {
    return 0
  }
  const pending = now.copyOnly ? 0 : now.trimBytes + now.scrapeBytes
  return now.size - now.untriedBytes - pending
})

const remainingText = computed(() => {
  const now = imaging.value
  // 只在复制这一轮估计：之后按扇区读坏区，速度说明不了什么
  if (now === null || now.round !== 'copy' || !now.bytesPerSecond) {
    return null
  }
  return formatRemaining(now.untriedBytes / now.bytesPerSecond)
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

const existingText = computed(() => {
  const found = existing.value
  if (found === null) {
    return ''
  }
  if (found.state !== 'ok') {
    return t(`diskImage.existing.${found.state}`)
  }
  const { readBytes, badBytes, untriedBytes, trimBytes, scrapeBytes } = found.progress
  return t('diskImage.existing.ok', {
    read: formatBytes(readBytes),
    size: formatBytes(current.value?.size ?? 0),
    bad: formatBytes(badBytes),
    left: formatBytes(untriedBytes + trimBytes + scrapeBytes),
  })
})

// 源或镜像路径一变，手上的结果就作废；停下一会儿再去看
watch([imageTarget, trimmedImagePath], () => {
  clearInspection()
  changeChoice.value = null
  selectedRegions.value = []
  redoSelected.value = false
  scheduleInspect(INSPECT_DELAY_MS)
})

// 做完、停下、失败之后再看一次：各段的状态更新，可以接着选剩下的
watch(status, (_status, before) => {
  // 开始时就失败了（例如这个位置刚好已经有了镜像）也再看一次
  if (before === 'starting' || before === 'running' || before === 'cancelling') {
    changeChoice.value = null
    redoSelected.value = false
    scheduleInspect(0)
    void history.load()
  }
})

onMounted(() => {
  // 从导航页切过来，焦点放到标题上，读屏软件从这里开始读
  titleElement.value?.focus()
  void history.load()
  void loadDisks()
})

onBeforeUnmount(() => {
  if (inspectTimer !== null) {
    clearTimeout(inspectTimer)
  }
})

function scheduleInspect(delay: number) {
  if (inspectTimer !== null) {
    clearTimeout(inspectTimer)
  }
  inspectTimer = setTimeout(() => {
    inspectTimer = null
    const target = imageTarget.value
    const path = trimmedImagePath.value
    if (target !== null && isAbsolutePath(path) && !isBusy.value) {
      void inspect(target, path)
    }
  }, delay)
}

async function loadDisks() {
  try {
    disks.value = await listDisks()
  } catch {
    // 只用来给做过的镜像认盘、给记录起名字；查不到时按磁盘编号显示
  }
}

function handleExit() {
  if (!isBusy.value) {
    emit('exit')
  }
}

function sourceLabel(target: JobTarget): string {
  if (target.kind === 'image') {
    return target.path
  }
  const disk = disks.value.find((candidate) => candidate.number === target.diskNumber)
  return disk === undefined
    ? sourceName(target)
    : t('diskImage.history.diskLabel', { name: diskName(disk), size: formatBytes(disk.size) })
}

function handleSubmit() {
  const target = imageTarget.value
  if (!canStart.value || target === null) {
    return
  }
  void start(
    { target, imagePath: trimmedImagePath.value, plan: plan.value, sourceLabel: sourceLabel(target) },
    canResume.value,
  )
}

function handleCancel() {
  void cancel()
}

/** 停下、源设备读不到之后接着做：同样的范围，不再清掉进度 */
function handleContinue() {
  void continueLast()
}

/** 源变过了，要另建一个镜像：清掉路径，让用户换个位置 */
function startOver() {
  imagePath.value = ''
  pathInput.value?.focus()
}

/** 系统的选择文件夹对话框，选好后在后面接上默认的文件名；用户取消时什么也不改 */
async function browseImageFolder() {
  const folder = await pickFolder(t('diskImage.output.pickTitle'))
  if (folder !== null) {
    imagePath.value = joinPath(folder, defaultImageName())
  }
}

/** 选用做过的一个镜像：填上路径，源换成当时的那个（物理盘按标识找现在的编号） */
async function useHistory(item: ImageHistoryItem) {
  historyNote.value = ''
  const { record } = item
  if (record.target.kind === 'image') {
    sourceKind.value = 'image'
    sourcePath.value = record.target.path
  } else {
    await loadDisks()
    const disk = disks.value.find((candidate) => candidate.identity === record.sourceIdentity)
    if (disk === undefined) {
      historyNote.value = t('diskImage.history.diskMissing', {
        source: record.sourceLabel || record.sourceIdentity,
      })
    } else {
      sourceKind.value = 'disk'
      selectedDisk.value = disk.number
    }
  }
  imagePath.value = record.imagePath
}

/** 镜像被移走了：选它现在的位置（要连同 .map 一起移过去），核对是同一个镜像才改记录 */
async function relocateHistory(item: ImageHistoryItem) {
  pickError.value = null
  let path: string | null
  try {
    path = await pickFile(t('diskImage.history.relocateTitle'))
  } catch (error) {
    pickError.value = toHostError(error)
    return
  }
  if (path === null) {
    return
  }
  const moved = await history.relocate(item.record.imagePath, path)
  if (moved !== null && trimmedImagePath.value === item.record.imagePath) {
    imagePath.value = moved.record.imagePath
  }
}

function removeHistory(item: ImageHistoryItem) {
  void history.remove(item.record.imagePath)
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

/** 盘符开头（C:\ 或 C:/）或网络路径（\\server\share） */
function isAbsolutePath(path: string): boolean {
  return /^[a-zA-Z]:[\\/]/.test(path) || /^[\\/]{2}[^\\/]/.test(path)
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

function changeText(change: { kind: string; number: number; before: ByteRange | null; after: ByteRange | null }): string {
  const where = (range: ByteRange | null) =>
    range === null ? '' : t('diskImage.changes.where', { offset: formatBytes(range.offset), size: formatBytes(range.length) })
  return t(`diskImage.changes.${change.kind}`, {
    number: change.number,
    before: where(change.before),
    after: where(change.after),
  })
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
          <li>{{ t('diskImage.guide.parts') }}</li>
          <li>{{ t('diskImage.guide.space') }}</li>
        </ul>
      </details>

      <details class="tool__guide">
        <summary class="tool__guide-summary">{{ t('diskImage.resumeRules.title') }}</summary>
        <ul class="tool__guide-list">
          <li>{{ t('diskImage.resumeRules.files') }}</li>
          <li>{{ t('diskImage.resumeRules.sameSource') }}</li>
          <li>{{ t('diskImage.resumeRules.noWrites') }}</li>
          <li>{{ t('diskImage.resumeRules.ntfs') }}</li>
          <li>{{ t('diskImage.resumeRules.crash') }}</li>
        </ul>
      </details>

      <section
        v-if="history.items.value.length > 0 || history.loadError.value"
        class="disk-image__section"
        aria-labelledby="disk-image-history-title"
      >
        <h3 id="disk-image-history-title" class="disk-image__section-title">
          {{ t('diskImage.history.title') }}
        </h3>
        <p v-if="history.loadError.value" class="wizard__error" role="alert">
          {{ formatHostError(history.loadError.value) }}
        </p>
        <ImageHistoryList
          :items="history.items.value"
          :disabled="isBusy"
          :current-path="trimmedImagePath"
          @use="useHistory"
          @relocate="relocateHistory"
          @remove="removeHistory"
        />
        <p class="wizard__hint">{{ t('diskImage.history.hint') }}</p>
        <p v-if="historyNote" class="wizard__hint wizard__hint--warning">{{ historyNote }}</p>
        <p v-if="history.actionError.value" class="wizard__error" role="alert">
          {{ formatHostError(history.actionError.value.error) }}
        </p>
        <p v-if="pickError" class="wizard__error" role="alert">{{ formatHostError(pickError) }}</p>
      </section>

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
              ref="pathInput"
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
          <p v-if="isInspecting" class="wizard__note" aria-live="polite">
            {{ sourceKind === 'disk' ? t('diskImage.inspect.loadingDisk') : t('diskImage.inspect.loading') }}
          </p>
          <p v-else-if="inspectFailure" class="wizard__error" role="alert">
            {{ formatHostError(inspectFailure) }}
          </p>
          <p v-if="current?.outputError" class="wizard__note">
            {{ t('diskImage.inspect.outputUnknown', { reason: current.outputError }) }}
          </p>
        </fieldset>

        <section
          v-if="existing"
          class="disk-image__section"
          aria-labelledby="disk-image-existing-title"
          aria-live="polite"
        >
          <h3 id="disk-image-existing-title" class="disk-image__section-title">
            {{ t('diskImage.existing.title') }}
          </h3>
          <p :class="isBlocked ? 'wizard__hint wizard__hint--warning' : 'wizard__note'">
            {{ existingText }}
          </p>
          <p v-if="existing.detail" class="wizard__note">{{ existing.detail }}</p>
          <p v-if="canResume && !existing.partitionsRecorded" class="wizard__note">
            {{ t('diskImage.existing.noSnapshot') }}
          </p>
          <template v-if="hasChanges">
            <p class="wizard__hint wizard__hint--warning">{{ t('diskImage.changes.title') }}</p>
            <ul class="disk-image__changes">
              <li v-for="change in existing.partitionChanges" :key="`${change.kind}-${change.number}`">
                {{ changeText(change) }}
              </li>
              <li v-for="region in mismatchedRegions" :key="region.offset">
                {{
                  t('diskImage.changes.mismatched', {
                    region: regionLine(region, current?.partitions ?? []),
                    count: region.check?.mismatched ?? 0,
                  }, region.check?.mismatched ?? 0)
                }}
              </li>
            </ul>
            <fieldset class="disk-image__choices" :disabled="isBusy">
              <legend class="wizard__note">{{ t('diskImage.changes.question') }}</legend>
              <label class="wizard__choice">
                <input v-model="changeChoice" type="radio" value="redo" />
                <span>
                  {{ t('diskImage.changes.redo') }}
                  <span class="wizard__note">{{ t('diskImage.changes.redoHint') }}</span>
                </span>
              </label>
              <label class="wizard__choice">
                <input v-model="changeChoice" type="radio" value="keep" />
                <span>
                  {{ t('diskImage.changes.keep') }}
                  <span class="wizard__note">{{ t('diskImage.changes.keepHint') }}</span>
                </span>
              </label>
              <div class="disk-image__resume">
                <button class="wizard__button wizard__button--secondary" type="button" @click="startOver">
                  {{ t('diskImage.changes.newImage') }}
                </button>
                <span class="wizard__note">{{ t('diskImage.changes.newImageHint') }}</span>
              </div>
            </fieldset>
          </template>
          <div v-if="isBlocked" class="disk-image__resume">
            <button class="wizard__button wizard__button--secondary" type="button" @click="startOver">
              {{ t('diskImage.existing.choosePlace') }}
            </button>
          </div>
        </section>

        <fieldset v-if="current && !isBlocked" class="disk-image__section" :disabled="isBusy">
          <legend class="disk-image__section-title">{{ t('diskImage.scope.title') }}</legend>
          <label class="wizard__choice">
            <input v-model="scopeMode" type="radio" value="whole" />
            <span>
              {{ t('diskImage.scope.whole', { size: formatBytes(current.size) }) }}
            </span>
          </label>
          <label class="wizard__choice" :class="{ 'disk-image__choice--disabled': !partialAllowed }">
            <input v-model="scopeMode" type="radio" value="select" :disabled="!partialAllowed" />
            <span>{{ t('diskImage.scope.select') }}</span>
          </label>
          <p v-if="partialBlockedReason" class="wizard__note">{{ partialBlockedReason }}</p>

          <template v-if="isSelecting">
            <ImageRegionPicker
              v-model="selectedRegions"
              :regions="current.regions"
              :partitions="current.partitions"
              :disabled="isBusy"
            />
            <p v-if="current.regions.length === 0" class="wizard__note">{{ t('diskImage.scope.noRegions') }}</p>
            <p class="wizard__hint">
              {{
                t('diskImage.scope.tableAreas', {
                  size: formatBytes(current.tableAreas.reduce((sum, area) => sum + area.length, 0)),
                })
              }}
            </p>
          </template>

          <label v-if="canResume" class="wizard__choice">
            <input v-model="redoSelected" type="checkbox" />
            <span>
              {{ isSelecting ? t('diskImage.scope.redoSelected') : t('diskImage.scope.redoWhole') }}
              <span class="wizard__note">{{ t('diskImage.scope.redoHint') }}</span>
            </span>
          </label>
          <label class="wizard__choice">
            <input v-model="copyOnly" type="checkbox" />
            <span>
              {{ t('diskImage.scope.copyOnly') }}
              <span class="wizard__note">{{ t('diskImage.scope.copyOnlyHint') }}</span>
            </span>
          </label>

          <p
            v-if="current.output && (!isSelecting || chosenRegions.length > 0)"
            :class="isShortOfSpace ? 'wizard__error' : 'wizard__note'"
          >
            {{
              partialAllowed
                ? t('diskImage.scope.space', {
                    needed: formatBytes(bytesToWrite),
                    free: formatBytes(current.output.freeBytes),
                  })
                : t('diskImage.scope.spaceWhole', {
                    size: formatBytes(current.size),
                    free: formatBytes(current.output.freeBytes),
                  })
            }}
          </p>
          <p v-if="nothingToDo" class="wizard__note">{{ t('diskImage.scope.nothingToDo') }}</p>
        </fieldset>

        <div class="wizard__nav">
          <span class="wizard__nav-gap"></span>
          <button v-if="!isBusy" class="wizard__button" type="submit" :disabled="!canStart">
            {{ canResume ? t('diskImage.continue') : t('diskImage.start') }}
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

        <div v-if="canContinue && lastRequest" class="disk-image__resume">
          <p class="wizard__note">
            {{ t('diskImage.continueHint', { path: lastRequest.imagePath }) }}
          </p>
          <button class="wizard__button" type="button" @click="handleContinue">
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
          <template v-if="finishedSummary.partial">
            <dt>{{ t('diskImage.result.device') }}</dt>
            <dd>
              {{
                t('diskImage.result.deviceValue', {
                  read: formatBytes(finishedSummary.deviceReadBytes),
                  size: formatBytes(finishedSummary.deviceSize),
                  left: formatBytes(finishedSummary.deviceUnfinishedBytes),
                })
              }}
            </dd>
          </template>
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

        <p v-if="finishedSummary.copyOnly && finishedSummary.unfinishedBytes > 0" class="wizard__hint wizard__hint--warning">
          {{ t('diskImage.result.copyOnlyLeft', { left: formatBytes(finishedSummary.unfinishedBytes) }) }}
        </p>
        <p v-if="finishedSummary.partial && finishedSummary.deviceUnfinishedBytes > 0" class="wizard__hint">
          {{ t('diskImage.result.partialLeft') }}
        </p>

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
  margin: 0;
  padding: 0 var(--space-xs);
  font-size: inherit;
  font-weight: 600;
}

.disk-image__resume {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-sm);
}

.disk-image__changes {
  margin: 0;
  padding-left: var(--space-lg);
}

.disk-image__choices {
  display: flex;
  flex-direction: column;
  gap: var(--space-xs);
  margin: 0;
  padding: 0;
  border: none;
}

.disk-image__choice--disabled {
  color: var(--color-text-muted);
  cursor: default;
}
</style>
