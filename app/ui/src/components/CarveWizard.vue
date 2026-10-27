<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import ContextMenu, { type MenuItem } from './ContextMenu.vue'
import FileTypePicker from './FileTypePicker.vue'
import RecoveredBrowser from './RecoveredBrowser.vue'
import RecoveredPreview from './RecoveredPreview.vue'
import SourcePicker from './SourcePicker.vue'
import { useCarve } from '../composables/useCarve'
import { useCopy } from '../composables/useCopy'
import { useFileTypeName } from '../composables/useFileTypeName'
import { useHostErrorText } from '../composables/useHostErrorText'
import {
  copyRecoveredFile,
  listFileTypes,
  openRecoveredFile,
  openRecoveredFileWith,
  pickDirectory,
  pickSaveFile,
  revealRecoveredFile,
  startExtract,
  toHostError,
  type ExtractItem,
  type FileType,
  type FoundFile,
  type HostError,
  type JobTarget,
} from '../services/host'
import { formatBytes } from '../utils/format'

const STEPS = ['source', 'formats', 'scan'] as const
type WizardStep = (typeof STEPS)[number]

/** 保存好的一份文件：保存它的任务，以及保存成的名字（另存为可能改了名、重名时加了序号） */
interface SavedCopy {
  jobId: string
  name: string
}

/** 要先有一份保存好的文件才能做的操作 */
type FileAction = 'open' | 'openWith' | 'copy'

/** 为打开、复制而存的副本，宿主拒绝写到程序目录时（例如程序就装在被扫描的盘上）改为请用户选位置 */
const OPEN_COPY_REFUSED = ['outputOnSource', 'outputUnavailable']

const emit = defineEmits<{ exit: [] }>()

const { t, locale } = useI18n()
const { formatHostError } = useHostErrorText()
const { fileTypeName } = useFileTypeName()
const {
  jobId,
  status,
  targetKind,
  done,
  total,
  percent,
  bytesPerSecond,
  foundCount,
  files,
  summary,
  failure,
  isUnresponsive,
  isBusy,
  start,
  cancel,
} = useCarve()

const titleElement = ref<HTMLHeadingElement | null>(null)
const browser = ref<InstanceType<typeof RecoveredBrowser> | null>(null)
const step = ref<WizardStep>('source')
const sourceKind = ref<JobTarget['kind']>('image')
const imagePath = ref('')
const selectedDisk = ref<number | null>(null)
const fileTypes = ref<FileType[]>([])
const isLoadingFileTypes = ref(false)
const fileTypesError = ref<HostError | null>(null)
const selectedExtensions = ref<string[]>([])
/** 高级选项：扫描时就把找到的文件直接保存到 outputDir；默认只扫描、什么都不写 */
const saveWhileScanning = ref(false)
const isAdvancedOpen = ref(false)
const outputDir = ref('')
const dialogError = ref<HostError | null>(null)
/** 发起扫描时是不是只扫描：之后回到上一步改选项，不影响正在显示的这次结果 */
const runScanOnly = ref(false)
/** 只扫描时勾选要另存的文件（按扫描时的文件名） */
const checkedNames = ref(new Set<string>())
/** 另存过的文件：扫描时的文件名 → 保存好的那一份 */
const savedCopies = ref(new Map<string, SavedCopy>())
/** 为打开、复制而存在程序目录里的副本；每存一次新的，宿主就清掉以前的，这里也只记最近一次的 */
const openCopies = ref(new Map<string, SavedCopy>())
/** 当前（或最近一次）保存任务是另存为，还是为打开、复制而存的副本 */
const saveKind = ref<'saveAs' | 'openCopy'>('saveAs')
/** 保存好之后要接着做的操作 */
const pendingAction = ref<{ file: FoundFile; action: FileAction } | null>(null)
/** 上次另存的目录，单个文件另存为时作为对话框的起点 */
const lastSaveDir = ref('')
const save = useCopy()
/** 结果列表里选中的文件，右侧显示它的详情与预览 */
const selectedFile = ref<FoundFile | null>(null)
/** 文件操作失败的原因与完成的提示，换选中的文件时清掉 */
const fileActionError = ref<HostError | null>(null)
const fileActionNotice = ref('')
/** 右键菜单：对哪个文件、弹在哪里 */
const menu = ref<{ file: FoundFile; x: number; y: number } | null>(null)

const stepIndex = computed(() => STEPS.indexOf(step.value))

const target = computed<JobTarget | null>(() => {
  if (sourceKind.value === 'image') {
    const path = imagePath.value.trim()
    return path === '' ? null : { kind: 'image', path }
  }
  return selectedDisk.value === null ? null : { kind: 'disk', diskNumber: selectedDisk.value }
})

const trimmedOutputDir = computed(() => outputDir.value.trim())

const canStart = computed(
  () =>
    target.value !== null &&
    selectedExtensions.value.length > 0 &&
    (!saveWhileScanning.value || trimmedOutputDir.value !== '') &&
    !isBusy.value &&
    !save.isBusy.value,
)

/** 勾选的文件，按找到的先后排列 */
const checkedFiles = computed(() =>
  checkedNames.value.size === 0 ? [] : files.value.filter((file) => checkedNames.value.has(file.name)),
)

/** 保存的结果按源上的偏移对回扫描到的文件 */
const filesByOffset = computed(() => new Map(files.value.map((file) => [file.offset, file])))

/** 扫描结束（完成或取消）、没有别的保存在进行时，才能从源上读出来保存：不和扫描抢同一块盘 */
const canExtract = computed(
  () =>
    jobId.value !== null &&
    (status.value === 'completed' || status.value === 'cancelled') &&
    !save.isBusy.value,
)

const canSaveChecked = computed(
  () => runScanOnly.value && checkedFiles.value.length > 0 && canExtract.value,
)

/** 只扫描的结果里还有没另存的文件：离开、重新扫描前要确认 */
const hasUnsaved = computed(
  () => runScanOnly.value && files.value.some((file) => !savedCopies.value.has(file.name)),
)

const saveStatusText = computed(() => {
  if (saveKind.value === 'openCopy') {
    return t('carve.save.preparing')
  }
  switch (save.status.value) {
    case 'starting':
      return t('carve.save.status.starting')
    case 'running':
    case 'cancelling':
      return t('carve.save.status.running', {
        done: formatBytes(save.done.value),
        total: formatBytes(save.total.value),
        percent: save.percent.value,
        count: save.succeeded.value.length,
      })
    case 'completed':
      return t('carve.save.status.completed')
    case 'failed':
      return t('carve.save.status.failed')
    case 'cancelled':
      return t('carve.save.status.cancelled')
  }
  return ''
})

/** 另存为的进度一直显示到下一次；为打开、复制存副本只在进行中显示一行，失败原因显示在详情里 */
const isSaveStatusShown = computed(
  () =>
    save.status.value !== 'idle' && (saveKind.value === 'saveAs' || save.isBusy.value),
)

const sourceText = computed(() => {
  const current = target.value
  if (current === null) {
    return ''
  }
  return current.kind === 'image'
    ? t('carve.run.sourceImage', { path: current.path })
    : t('carve.run.sourceDisk', { number: current.diskNumber })
})

const selectedTypeNames = computed(() => {
  const names = fileTypes.value
    .filter((type) => selectedExtensions.value.includes(type.extension))
    .map((type) => fileTypeName(type.extension, type.description))
  return new Intl.ListFormat(locale.value, { type: 'conjunction' }).format(names)
})

const selectedTypeName = computed(() =>
  selectedFile.value === null ? '' : typeNameOf(selectedFile.value),
)

/** 启动失败（例如输出目录在源盘上）时还没有任何结果，不显示空的文件列表 */
const hasResults = computed(
  () =>
    status.value === 'running' ||
    status.value === 'cancelling' ||
    foundCount.value > 0 ||
    summary.value !== null,
)

const statusText = computed(() => {
  switch (status.value) {
    case 'idle':
      return ''
    case 'starting':
      return targetKind.value === 'disk'
        ? t('carve.run.status.startingDisk')
        : t('carve.run.status.starting')
    case 'running':
    case 'cancelling':
      return t('carve.run.status.running', {
        done: formatBytes(done.value),
        total: formatBytes(total.value),
        percent: percent.value,
        rate: bytesPerSecond.value === null ? '-' : formatBytes(bytesPerSecond.value),
      })
    case 'completed':
      return t('carve.run.status.completed')
    case 'failed':
      return t('carve.run.status.failed')
    case 'cancelled':
      return t('carve.run.status.cancelled')
  }
  return ''
})

const menuItems = computed<MenuItem[]>(() => {
  const file = menu.value?.file
  if (file === undefined) {
    return []
  }
  // 还没有保存好的一份时要先从源上读出来，扫描或保存进行中不能做
  const needsCopy = copyOf(file) === null && !canExtract.value
  return [
    { key: 'open', label: t('carve.menu.open'), disabled: needsCopy },
    { key: 'openWith', label: t('carve.menu.openWith'), disabled: needsCopy },
    { key: 'copy', label: t('carve.menu.copy'), disabled: needsCopy },
    { key: 'saveAs', label: t('carve.menu.saveAs'), disabled: !canExtract.value },
    { key: 'reveal', label: t('carve.menu.reveal'), disabled: !isSavedByUser(file) },
  ]
})

watch(selectedFile, () => {
  fileActionError.value = null
  fileActionNotice.value = ''
})

// 保存好的文件按偏移对回扫描到的文件，记下保存在哪、叫什么
watch(save.copied, (copied) => {
  const saveJobId = save.jobId.value
  if (saveJobId === null) {
    return
  }
  const copies = saveKind.value === 'saveAs' ? savedCopies : openCopies
  const next = new Map(copies.value)
  for (const copiedFile of copied) {
    const file = copiedFile.offset === undefined ? undefined : filesByOffset.value.get(copiedFile.offset)
    if (copiedFile.error === null && file !== undefined) {
      next.set(file.name, { jobId: saveJobId, name: copiedFile.path })
    }
  }
  copies.value = next
})

// 为打开、复制而保存的文件保存好了就接着做；副本存不到程序目录时请用户选位置，没保存成功时说明原因
watch(save.status, async (current) => {
  const pending = pendingAction.value
  if (pending === null || !['completed', 'failed', 'cancelled'].includes(current)) {
    return
  }
  pendingAction.value = null
  const copy = copyOf(pending.file)
  if (copy !== null) {
    await runAction(pending.action, copy)
    return
  }
  const failureCode = save.failure.value?.code ?? ''
  if (saveKind.value === 'openCopy' && OPEN_COPY_REFUSED.includes(failureCode)) {
    const dir = await pickFolder(t('carve.save.fallbackTitle'))
    if (dir !== null) {
      pendingAction.value = pending
      await saveFiles([pending.file], dir, 'saveAs')
    }
    return
  }
  fileActionError.value = save.failure.value ?? {
    code: 'jobFailed',
    detail: save.failures.value[0]?.error ?? '',
  }
})

onMounted(() => {
  // 从首页切过来，焦点放到向导标题上，读屏软件从这里开始读
  titleElement.value?.focus()
  void loadFileTypes()
})

/** 只扫描的结果还没全部另存时先确认：离开或重新扫描后，这些结果就没了 */
function confirmDiscard(): boolean {
  return !hasUnsaved.value || window.confirm(t('carve.save.discardConfirm'))
}

function handleExit() {
  if (!isBusy.value && !save.isBusy.value && confirmDiscard()) {
    emit('exit')
  }
}

function goBack() {
  const previous = STEPS[stepIndex.value - 1]
  if (previous !== undefined && !isBusy.value && !save.isBusy.value) {
    dialogError.value = null
    step.value = previous
  }
}

/** 表单提交（含在路径框里按回车）：来源这一步进入下一步，之后开始（或重新开始）扫描 */
function handleSubmit() {
  if (step.value === 'source') {
    if (target.value !== null) {
      dialogError.value = null
      step.value = 'formats'
    }
    return
  }
  handleStart()
}

function handleStart() {
  if (!canStart.value || target.value === null || !confirmDiscard()) {
    return
  }
  dialogError.value = null
  runScanOnly.value = !saveWhileScanning.value
  selectedFile.value = null
  checkedNames.value = new Set()
  savedCopies.value = new Map()
  openCopies.value = new Map()
  step.value = 'scan'
  void start(target.value, runScanOnly.value ? null : trimmedOutputDir.value, [
    ...selectedExtensions.value,
  ])
}

function handleCancel() {
  void cancel()
}

/** 已经保存好的一份：边扫边保存的都在扫描任务的输出目录里，只扫描的要另存过或存过副本才有 */
function copyOf(file: FoundFile): SavedCopy | null {
  if (!runScanOnly.value) {
    return jobId.value === null ? null : { jobId: jobId.value, name: file.name }
  }
  return savedCopies.value.get(file.name) ?? openCopies.value.get(file.name) ?? null
}

/** 文件已经在用户的保存位置（不算为打开、复制存在程序目录里的副本） */
function isSavedByUser(file: FoundFile): boolean {
  return !runScanOnly.value || savedCopies.value.has(file.name)
}

/** 打开、选择打开方式、复制：还没有保存好的一份时，先存一份副本到程序目录，存好再做 */
async function performFileAction(file: FoundFile, action: FileAction) {
  selectedFile.value = file
  fileActionError.value = null
  fileActionNotice.value = ''
  const copy = copyOf(file)
  if (copy !== null) {
    await runAction(action, copy)
    return
  }
  if (!canExtract.value) {
    fileActionError.value = { code: 'scanStillRunning', detail: file.name }
    return
  }
  pendingAction.value = { file, action }
  await saveFiles([file], null, 'openCopy')
}

async function runAction(action: FileAction, copy: SavedCopy) {
  try {
    switch (action) {
      case 'open':
        await openRecoveredFile(copy.jobId, copy.name)
        break
      case 'openWith':
        await openRecoveredFileWith(copy.jobId, copy.name)
        break
      case 'copy':
        await copyRecoveredFile(copy.jobId, copy.name)
        fileActionNotice.value = t('carve.menu.copied', { name: copy.name })
        break
    }
  } catch (error) {
    fileActionError.value = toHostError(error)
  }
}

async function revealFile(file: FoundFile) {
  const copy = runScanOnly.value ? savedCopies.value.get(file.name) : copyOf(file)
  if (copy === undefined || copy === null) {
    return
  }
  selectedFile.value = file
  fileActionError.value = null
  try {
    await revealRecoveredFile(copy.jobId, copy.name)
  } catch (error) {
    fileActionError.value = toHostError(error)
  }
}

/** 列表下方的「另存为」：把勾选的文件存进选的文件夹，名字与扫描时一样 */
async function handleSaveChecked() {
  if (!canSaveChecked.value) {
    return
  }
  const dir = await pickFolder(t('carve.save.pickTitle'))
  if (dir !== null) {
    await saveFiles(checkedFiles.value, dir, 'saveAs')
  }
}

/** 单个文件另存为：系统的另存为对话框，可以改名 */
async function saveFileAs(file: FoundFile) {
  if (!canExtract.value) {
    return
  }
  selectedFile.value = file
  dialogError.value = null
  try {
    const path = await pickSaveFile(
      t('carve.save.pickFileTitle'),
      lastSaveDir.value === '' ? file.name : joinPath(lastSaveDir.value, file.name),
      { name: typeNameOf(file), extension: file.extension },
    )
    if (path !== null) {
      const { dir, name } = splitPath(path)
      await saveFiles([file], dir, 'saveAs', name)
    }
  } catch (error) {
    dialogError.value = toHostError(error)
  }
}

/** `dir` 为 null 时是为打开、复制存副本，宿主存到程序目录并清掉以前的副本 */
async function saveFiles(
  selected: FoundFile[],
  dir: string | null,
  kind: 'saveAs' | 'openCopy',
  name?: string,
) {
  const scanJobId = jobId.value
  if (scanJobId === null) {
    return
  }
  const items: ExtractItem[] = selected.map((file) => ({
    offset: file.offset,
    length: file.length,
    extension: file.extension,
    ...(name === undefined ? {} : { name }),
  }))
  saveKind.value = kind
  if (kind === 'openCopy') {
    openCopies.value = new Map()
  } else if (dir !== null) {
    lastSaveDir.value = dir
  }
  await save.start((saveJobId) => startExtract(saveJobId, scanJobId, items, dir))
}

function openMenu(file: FoundFile, point: { x: number; y: number }) {
  selectedFile.value = file
  menu.value = { file, ...point }
}

function closeMenu() {
  menu.value = null
  browser.value?.focus()
}

function handleMenuSelect(key: string) {
  const file = menu.value?.file
  closeMenu()
  if (file === undefined) {
    return
  }
  switch (key) {
    case 'open':
    case 'openWith':
    case 'copy':
      void performFileAction(file, key)
      break
    case 'saveAs':
      void saveFileAs(file)
      break
    case 'reveal':
      void revealFile(file)
      break
  }
}

function typeNameOf(file: FoundFile): string {
  const description = fileTypes.value.find((type) => type.extension === file.extension)?.description
  return fileTypeName(file.extension, description)
}

function joinPath(dir: string, name: string): string {
  return /[\\/]$/.test(dir) ? `${dir}${name}` : `${dir}\\${name}`
}

/** 拆成目录与文件名；目录是盘符根目录时保留结尾的分隔符（`F:` 指的是 F 盘的当前目录，不是根目录） */
function splitPath(path: string): { dir: string; name: string } {
  const index = Math.max(path.lastIndexOf('\\'), path.lastIndexOf('/'))
  const dir = path.slice(0, index)
  return { dir: dir.endsWith(':') ? `${dir}\\` : dir, name: path.slice(index + 1) }
}

async function loadFileTypes() {
  isLoadingFileTypes.value = true
  fileTypesError.value = null
  try {
    const types = await listFileTypes()
    fileTypes.value = types
    selectedExtensions.value = types
      .filter((type) => type.enabledByDefault)
      .map((type) => type.extension)
  } catch (error) {
    fileTypesError.value = toHostError(error)
  } finally {
    isLoadingFileTypes.value = false
  }
}

async function browseOutputDir() {
  const dir = await pickFolder(t('carve.output.pickTitle'))
  if (dir !== null) {
    outputDir.value = dir
  }
}

/** 系统的选择文件夹对话框；用户取消或对话框出错时返回 null */
async function pickFolder(title: string): Promise<string | null> {
  dialogError.value = null
  try {
    return await pickDirectory(title)
  } catch (error) {
    dialogError.value = toHostError(error)
    return null
  }
}

function handleAdvancedToggle(event: Event) {
  isAdvancedOpen.value = (event.target as HTMLDetailsElement).open
}
</script>

<template>
  <section class="wizard" aria-labelledby="carve-wizard-title">
    <header class="wizard__header">
      <button
        class="wizard__button wizard__button--secondary"
        type="button"
        :disabled="isBusy"
        @click="handleExit"
      >
        {{ t('nav.back') }}
      </button>
      <h2 id="carve-wizard-title" ref="titleElement" class="wizard__title" tabindex="-1">
        {{ t('features.carve.name') }}
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
            {{ t(`carve.steps.${name}`) }}
          </li>
        </ol>
        <div class="wizard__nav">
          <button
            v-if="stepIndex > 0"
            class="wizard__button wizard__button--secondary"
            type="button"
            :disabled="isBusy || save.isBusy.value"
            @click="goBack"
          >
            {{ t('carve.previous') }}
          </button>
          <button
            v-if="step === 'source'"
            class="wizard__button"
            type="submit"
            :disabled="target === null"
          >
            {{ t('carve.next') }}
          </button>
          <button
            v-else-if="step === 'formats'"
            class="wizard__button"
            type="submit"
            :disabled="!canStart"
          >
            {{ t('carve.formats.start') }}
          </button>
          <button v-else-if="!isBusy" class="wizard__button" type="submit" :disabled="!canStart">
            {{ t('carve.run.restart') }}
          </button>
          <button
            v-else
            class="wizard__button wizard__button--secondary"
            type="button"
            :disabled="status !== 'running'"
            @click="handleCancel"
          >
            {{ status === 'cancelling' ? t('carve.run.cancelling') : t('carve.run.cancel') }}
          </button>
        </div>
      </div>

      <div v-if="step === 'source'" class="wizard__panel">
        <SourcePicker
          v-model:kind="sourceKind"
          v-model:image-path="imagePath"
          v-model:disk="selectedDisk"
          :disk-hint="t('carve.source.diskHint')"
        />
      </div>

      <div v-else-if="step === 'formats'" class="wizard__panel">
        <p v-if="isLoadingFileTypes" class="wizard__note">{{ t('carve.formats.loading') }}</p>
        <div v-else-if="fileTypesError" class="wizard__retry">
          <p class="wizard__error" role="alert">{{ formatHostError(fileTypesError) }}</p>
          <button
            class="wizard__button wizard__button--secondary"
            type="button"
            @click="loadFileTypes"
          >
            {{ t('carve.formats.retry') }}
          </button>
        </div>
        <template v-else>
          <FileTypePicker v-model="selectedExtensions" :types="fileTypes" :disabled="false" />
          <p class="wizard__hint">{{ t('carve.formats.hint') }}</p>
        </template>

        <details class="carve-wizard__advanced" :open="isAdvancedOpen" @toggle="handleAdvancedToggle">
          <summary class="carve-wizard__advanced-summary">
            {{ t('carve.formats.advanced') }}
            <span v-if="saveWhileScanning" class="carve-wizard__advanced-state">
              {{ t('carve.formats.advancedOn') }}
            </span>
          </summary>
          <div class="carve-wizard__advanced-body">
            <label class="carve-wizard__option">
              <input v-model="saveWhileScanning" type="checkbox" />
              {{ t('carve.formats.saveWhileScanning') }}
            </label>
            <p class="wizard__hint">{{ t('carve.formats.saveWhileScanningHint') }}</p>
            <template v-if="saveWhileScanning">
              <div class="wizard__path">
                <label class="wizard__label" for="carve-output-dir">
                  {{ t('carve.output.label') }}
                </label>
                <input
                  id="carve-output-dir"
                  v-model="outputDir"
                  class="wizard__input"
                  type="text"
                  spellcheck="false"
                  autocomplete="off"
                  :placeholder="t('carve.output.placeholder')"
                />
                <button
                  class="wizard__button wizard__button--secondary"
                  type="button"
                  @click="browseOutputDir"
                >
                  {{ t('carve.browseFolder') }}
                </button>
              </div>
              <p class="wizard__hint">{{ t('carve.output.hint') }}</p>
              <p v-if="sourceKind === 'disk'" class="wizard__hint wizard__hint--warning">
                {{ t('carve.output.diskHint') }}
              </p>
            </template>
          </div>
        </details>
      </div>

      <div v-else class="wizard__panel">
        <dl class="wizard__details">
          <dt>{{ t('carve.run.source') }}</dt>
          <dd class="wizard__path-text">{{ sourceText }}</dd>
          <dt>{{ t('carve.run.formats') }}</dt>
          <dd>{{ selectedTypeNames }}</dd>
          <dt>{{ t('carve.run.output') }}</dt>
          <dd v-if="runScanOnly">{{ t('carve.run.outputAfterScan') }}</dd>
          <dd v-else class="wizard__path-text">{{ trimmedOutputDir }}</dd>
        </dl>

        <div v-if="status !== 'idle'" class="wizard__job" aria-live="polite">
          <progress class="wizard__progress" :value="done" :max="total || 1"></progress>
          <p class="wizard__status">{{ statusText }}</p>
          <p v-if="isUnresponsive" class="wizard__error" role="alert">
            {{ t('carve.run.unresponsive') }}
          </p>
          <p v-if="failure" class="wizard__error" role="alert">
            {{ formatHostError(failure) }}
          </p>
          <p v-if="status === 'cancelled'" class="wizard__hint">
            {{ runScanOnly ? t('carve.run.cancelledScanOnlyNote') : t('carve.run.cancelledNote') }}
          </p>

          <div v-if="hasResults" class="wizard__results">
            <dl v-if="summary && status === 'completed'" class="carve-wizard__report">
              <div class="carve-wizard__report-item">
                <dt>{{ t('carve.run.report.files') }}</dt>
                <dd>
                  {{
                    t('carve.run.report.filesValue', {
                      count: summary.files,
                      bytes: formatBytes(summary.bytes),
                    })
                  }}
                </dd>
              </div>
              <div class="carve-wizard__report-item">
                <dt>{{ t('carve.run.report.unreadable') }}</dt>
                <dd>{{ formatBytes(summary.unreadableBytes) }}</dd>
              </div>
              <div class="carve-wizard__report-item">
                <dt>{{ t('carve.run.report.elapsed') }}</dt>
                <dd>
                  {{
                    t('carve.run.report.elapsedValue', {
                      seconds: (summary.elapsedMs / 1000).toFixed(1),
                    })
                  }}
                </dd>
              </div>
              <div v-if="summary.outputDir" class="carve-wizard__report-item">
                <dt>{{ t('carve.run.report.outputDir') }}</dt>
                <dd class="wizard__path-text">{{ summary.outputDir }}</dd>
              </div>
            </dl>

            <div class="carve-wizard__files">
              <RecoveredBrowser
                ref="browser"
                v-model:selected="selectedFile"
                v-model:checked="checkedNames"
                class="carve-wizard__pane"
                :files="files"
                :file-types="fileTypes"
                :device-size="total"
                :selectable="runScanOnly"
                @open="performFileAction($event, 'open')"
                @menu="openMenu"
              >
                <template v-if="runScanOnly" #actions>
                  <div class="carve-wizard__save-as">
                    <button
                      class="wizard__button"
                      type="button"
                      :disabled="!canSaveChecked"
                      @click="handleSaveChecked"
                    >
                      {{ t('carve.save.saveAs', { count: checkedFiles.length }) }}
                    </button>
                    <span class="wizard__note">
                      {{ t('carve.save.savedCount', { saved: savedCopies.size, total: files.length }) }}
                    </span>
                  </div>
                </template>
              </RecoveredBrowser>
              <RecoveredPreview
                class="carve-wizard__pane"
                :job-id="jobId"
                :file="selectedFile"
                :type-name="selectedTypeName"
                :action-error="fileActionError"
                :scan-only="runScanOnly"
                :is-saved="selectedFile !== null && isSavedByUser(selectedFile)"
                :can-open="selectedFile !== null && (copyOf(selectedFile) !== null || canExtract)"
                :can-save-as="canExtract"
                @open="performFileAction($event, 'open')"
                @save-as="saveFileAs"
                @reveal="revealFile"
              />
            </div>

            <p v-if="fileActionNotice" class="wizard__hint" role="status">{{ fileActionNotice }}</p>

            <div v-if="isSaveStatusShown" class="wizard__job" aria-live="polite">
              <template v-if="saveKind === 'saveAs'">
                <div class="carve-wizard__save-progress">
                  <progress
                    class="wizard__progress"
                    :value="save.done.value"
                    :max="save.total.value || 1"
                  ></progress>
                  <button
                    v-if="save.isBusy.value"
                    class="wizard__button wizard__button--secondary"
                    type="button"
                    :disabled="save.status.value !== 'running'"
                    @click="save.cancel"
                  >
                    {{
                      save.status.value === 'cancelling'
                        ? t('carve.run.cancelling')
                        : t('carve.run.cancel')
                    }}
                  </button>
                </div>
                <p class="wizard__status">{{ saveStatusText }}</p>
                <p v-if="save.failure.value" class="wizard__error" role="alert">
                  {{ formatHostError(save.failure.value) }}
                </p>
                <template v-if="save.failures.value.length > 0">
                  <p class="wizard__error">
                    {{ t('carve.save.failedCount', { count: save.failures.value.length }) }}
                  </p>
                  <ul class="carve-wizard__failures">
                    <li v-for="failed in save.failures.value.slice(0, 20)" :key="failed.path">
                      {{ t('carve.save.failedItem', { name: failed.path, reason: failed.error }) }}
                    </li>
                  </ul>
                </template>
                <p v-if="save.summary.value" class="wizard__hint wizard__path-text">
                  {{
                    t('carve.save.summary', {
                      count: save.summary.value.files,
                      bytes: formatBytes(save.summary.value.bytes),
                      dir: save.summary.value.outputDir,
                    })
                  }}
                </p>
              </template>
              <p v-else class="wizard__status">{{ saveStatusText }}</p>
            </div>
          </div>
        </div>
      </div>

      <p v-if="dialogError" class="wizard__error" role="alert">
        {{ formatHostError(dialogError) }}
      </p>
    </form>

    <ContextMenu
      v-if="menu"
      :items="menuItems"
      :x="menu.x"
      :y="menu.y"
      :label="t('carve.menu.label', { name: menu.file.name })"
      @select="handleMenuSelect"
      @close="closeMenu"
    />
  </section>
</template>

<style scoped>
.carve-wizard__advanced {
  padding: var(--space-sm) var(--space-md);
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-card);
}

.carve-wizard__advanced-summary {
  font-weight: 600;
  cursor: pointer;
}

.carve-wizard__advanced-state {
  margin-left: var(--space-sm);
  color: var(--color-accent);
  font-weight: 400;
}

.carve-wizard__advanced-body {
  display: flex;
  flex-direction: column;
  gap: var(--space-sm);
  margin-top: var(--space-sm);
}

.carve-wizard__option {
  display: flex;
  align-items: center;
  gap: var(--space-xs);
  cursor: pointer;
}

/* 扫描报告排成一行，放不下时折行 */
.carve-wizard__report {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-xs) var(--space-lg);
  margin: 0;
}

.carve-wizard__report-item {
  display: flex;
  gap: var(--space-sm);
  min-width: 0;
}

.carve-wizard__report-item dt {
  color: var(--color-text-muted);
}

.carve-wizard__report-item dd {
  margin: 0;
  font-variant-numeric: tabular-nums;
}

/* 文件列表与文件详情两栏，都分标题、主体、底部三行：标题、列表框与详情框、提示各自对齐。
   右栏略宽，十六进制预览的 ASCII 列在 1200px 宽的窗口里不用横向滚动 */
.carve-wizard__files {
  display: grid;
  grid-template-columns: minmax(0, 9fr) minmax(0, 11fr);
  grid-template-rows: auto auto auto;
  gap: var(--space-sm) var(--space-lg);
}

.carve-wizard__files > .carve-wizard__pane {
  display: grid;
  grid-row: span 3;
  grid-template-rows: subgrid;
}

.carve-wizard__save-as {
  display: flex;
  flex: none;
  flex-direction: column;
  align-items: flex-end;
  gap: var(--space-xs);
}

.carve-wizard__save-progress {
  display: flex;
  align-items: center;
  gap: var(--space-sm);
}

.carve-wizard__failures {
  max-height: 160px;
  margin: 0;
  padding-left: var(--space-lg);
  overflow-y: auto;
  font-size: var(--font-size-small);
  overflow-wrap: anywhere;
}
</style>
