<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import FileTypePicker from './FileTypePicker.vue'
import RecoveredBrowser from './RecoveredBrowser.vue'
import RecoveredPreview from './RecoveredPreview.vue'
import SourcePicker from './SourcePicker.vue'
import { useCarve } from '../composables/useCarve'
import { useCopy } from '../composables/useCopy'
import { useFileTypeName } from '../composables/useFileTypeName'
import { useHostErrorText } from '../composables/useHostErrorText'
import {
  listFileTypes,
  openRecoveredFile,
  pickDirectory,
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

const STEPS = ['source', 'formats', 'output', 'run'] as const
type WizardStep = (typeof STEPS)[number]

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
  foundByType,
  files,
  summary,
  failure,
  isUnresponsive,
  isBusy,
  start,
  cancel,
} = useCarve()

const titleElement = ref<HTMLHeadingElement | null>(null)
const step = ref<WizardStep>('source')
const sourceKind = ref<JobTarget['kind']>('image')
const imagePath = ref('')
const selectedDisk = ref<number | null>(null)
const fileTypes = ref<FileType[]>([])
const isLoadingFileTypes = ref(false)
const fileTypesError = ref<HostError | null>(null)
const selectedExtensions = ref<string[]>([])
const outputDir = ref('')
/** 保存方式：边扫边保存到输出目录，或先只扫描、扫完再选要保存的文件 */
const saveMode = ref<'whileScanning' | 'afterScan'>('whileScanning')
const dialogError = ref<HostError | null>(null)
/** 发起扫描时是不是只扫描：之后回到上一步改选项，不影响正在显示的这次结果 */
const runScanOnly = ref(false)
/** 只扫描时勾选要保存的文件（按文件名） */
const checkedNames = ref(new Set<string>())
/** 只扫描时已经保存好的文件：文件名 → 保存它的任务 ID */
const savedBy = ref(new Map<string, string>())
/** 当前的保存任务是为「打开」而保存的这个文件，保存好就打开它 */
const openAfterSave = ref<string | null>(null)
const save = useCopy()
/** 发起恢复时勾选的格式：之后回到上一步改勾选，不影响正在显示的这次结果 */
const runExtensions = ref<string[]>([])
/** 结果列表里选中的文件，右侧显示它的详情与预览 */
const selectedFile = ref<FoundFile | null>(null)
/** 打开或在文件夹中显示失败的原因，换选中的文件时清掉 */
const fileActionError = ref<HostError | null>(null)

const stepIndex = computed(() => STEPS.indexOf(step.value))

const target = computed<JobTarget | null>(() => {
  if (sourceKind.value === 'image') {
    const path = imagePath.value.trim()
    return path === '' ? null : { kind: 'image', path }
  }
  return selectedDisk.value === null ? null : { kind: 'disk', diskNumber: selectedDisk.value }
})

const trimmedOutputDir = computed(() => outputDir.value.trim())

const canGoNext = computed(() => {
  switch (step.value) {
    case 'source':
      return target.value !== null
    case 'formats':
      return selectedExtensions.value.length > 0
    case 'output':
      return saveMode.value === 'afterScan' || trimmedOutputDir.value !== ''
    case 'run':
      return false
  }
  return false
})

const canStart = computed(
  () =>
    target.value !== null &&
    selectedExtensions.value.length > 0 &&
    (saveMode.value === 'afterScan' || trimmedOutputDir.value !== '') &&
    !isBusy.value &&
    !save.isBusy.value,
)

/** 勾选的文件，按找到的先后排列 */
const checkedFiles = computed(() =>
  checkedNames.value.size === 0 ? [] : files.value.filter((file) => checkedNames.value.has(file.name)),
)

/** 扫描结束（完成或取消）后才能保存：保存要回到源上读，不和扫描抢同一块盘 */
const canSave = computed(
  () =>
    runScanOnly.value &&
    checkedFiles.value.length > 0 &&
    jobId.value !== null &&
    (status.value === 'completed' || status.value === 'cancelled') &&
    !save.isBusy.value,
)

/** 只扫描的结果里还有没保存的文件：离开、重新扫描前要确认 */
const hasUnsaved = computed(
  () => runScanOnly.value && files.value.some((file) => !savedBy.value.has(file.name)),
)

const saveStatusText = computed(() => {
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

/** 按格式统计：运行中边找边累计，结束后以恢复报告为准；行按格式列表的顺序排列 */
const typeRows = computed(() => {
  const totals = summary.value?.byType ?? foundByType.value
  return fileTypes.value
    .filter((type) => runExtensions.value.includes(type.extension))
    .map((type) => ({
      extension: type.extension,
      name: fileTypeName(type.extension, type.description),
      totals: totals[type.extension] ?? { count: 0, bytes: 0 },
    }))
})

const selectedTypeName = computed(() => {
  const file = selectedFile.value
  if (file === null) {
    return ''
  }
  const description = fileTypes.value.find((type) => type.extension === file.extension)?.description
  return fileTypeName(file.extension, description)
})

/** 启动失败（例如输出目录在源盘上）时还没有任何结果，不显示空的统计表 */
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

watch(selectedFile, () => {
  fileActionError.value = null
})

// 保存好的文件记下是哪个任务保存的，之后按它打开、在文件夹中显示
watch(save.copied, (copied) => {
  const saveJobId = save.jobId.value
  if (saveJobId === null) {
    return
  }
  const next = new Map(savedBy.value)
  for (const file of copied) {
    if (file.error === null) {
      next.set(file.path, saveJobId)
    }
  }
  savedBy.value = next
})

// 为「打开」而保存的文件保存好了就打开；没保存成功时说明原因
watch(save.status, async (current) => {
  const name = openAfterSave.value
  if (name === null || !['completed', 'failed', 'cancelled'].includes(current)) {
    return
  }
  openAfterSave.value = null
  const saveJobId = savedBy.value.get(name)
  if (saveJobId === undefined) {
    fileActionError.value = save.failure.value ?? {
      code: 'jobFailed',
      detail: save.failures.value[0]?.error ?? '',
    }
    return
  }
  try {
    await openRecoveredFile(saveJobId, name)
  } catch (error) {
    fileActionError.value = toHostError(error)
  }
})

onMounted(() => {
  // 从首页切过来，焦点放到向导标题上，读屏软件从这里开始读
  titleElement.value?.focus()
  void loadFileTypes()
})

/** 只扫描的结果还没全部保存时先确认：离开或重新扫描后，这些结果就没了 */
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

/** 表单提交（含在路径框里按回车）：前三步进入下一步，最后一步开始恢复 */
function handleSubmit() {
  if (step.value === 'run') {
    handleStart()
    return
  }
  const next = STEPS[stepIndex.value + 1]
  if (next !== undefined && canGoNext.value) {
    dialogError.value = null
    step.value = next
  }
}

function handleStart() {
  if (!canStart.value || target.value === null || !confirmDiscard()) {
    return
  }
  runExtensions.value = [...selectedExtensions.value]
  runScanOnly.value = saveMode.value === 'afterScan'
  selectedFile.value = null
  checkedNames.value = new Set()
  savedBy.value = new Map()
  void start(target.value, runScanOnly.value ? null : trimmedOutputDir.value, runExtensions.value)
}

function handleCancel() {
  void cancel()
}

/** 结果列表里双击文件、或点「用默认程序打开」。只扫描时没保存过的先保存到保存位置，保存好再打开 */
async function openFile(file: FoundFile) {
  if (!runScanOnly.value || savedBy.value.has(file.name)) {
    await runFileAction(file, openRecoveredFile)
    return
  }
  selectedFile.value = file
  fileActionError.value = null
  if (isBusy.value || save.isBusy.value) {
    return
  }
  const dir = await ensureOutputDir()
  if (dir === null) {
    return
  }
  openAfterSave.value = file.name
  await saveFiles([file], dir)
}

async function revealFile(file: FoundFile) {
  await runFileAction(file, revealRecoveredFile)
}

/** 对输出目录里的文件做操作：边扫边保存的按扫描任务找，只扫描的按保存它的任务找 */
async function runFileAction(
  file: FoundFile,
  action: (jobId: string, name: string) => Promise<void>,
) {
  const ownerJobId = runScanOnly.value ? (savedBy.value.get(file.name) ?? null) : jobId.value
  if (ownerJobId === null) {
    return
  }
  selectedFile.value = file
  fileActionError.value = null
  try {
    await action(ownerJobId, file.name)
  } catch (error) {
    fileActionError.value = toHostError(error)
  }
}

async function handleSave() {
  if (!canSave.value) {
    return
  }
  const dir = await ensureOutputDir()
  if (dir !== null) {
    await saveFiles(checkedFiles.value, dir)
  }
}

async function saveFiles(selected: FoundFile[], dir: string) {
  const scanJobId = jobId.value
  if (scanJobId === null) {
    return
  }
  const items: ExtractItem[] = selected.map((file) => ({
    offset: file.offset,
    length: file.length,
    extension: file.extension,
  }))
  await save.start((saveJobId) => startExtract(saveJobId, scanJobId, items, dir))
  if (save.status.value === 'failed' && openAfterSave.value !== null) {
    openAfterSave.value = null
    fileActionError.value = save.failure.value
  }
}

/** 还没选保存位置时先弹出选择文件夹对话框；用户取消时返回 null */
async function ensureOutputDir(): Promise<string | null> {
  if (trimmedOutputDir.value === '') {
    await browseOutputDir()
  }
  return trimmedOutputDir.value === '' ? null : trimmedOutputDir.value
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
  await pickPath(() => pickDirectory(t('carve.output.pickTitle')), (path) => {
    outputDir.value = path
  })
}

/** 打开系统对话框；用户取消时什么也不改 */
async function pickPath(pick: () => Promise<string | null>, apply: (path: string) => void) {
  dialogError.value = null
  try {
    const path = await pick()
    if (path !== null) {
      apply(path)
    }
  } catch (error) {
    dialogError.value = toHostError(error)
  }
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
        {{ t('carve.exit') }}
      </button>
      <h2 id="carve-wizard-title" ref="titleElement" class="wizard__title" tabindex="-1">
        {{ t('scenarios.carve') }}
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
        {{ t(`carve.steps.${name}`) }}
      </li>
    </ol>

    <form class="wizard__form" @submit.prevent="handleSubmit">
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
      </div>

      <div v-else-if="step === 'output'" class="wizard__panel">
        <fieldset class="carve-wizard__modes">
          <legend class="carve-wizard__modes-legend">{{ t('carve.output.modeLegend') }}</legend>
          <label class="carve-wizard__mode">
            <input v-model="saveMode" type="radio" name="carve-save-mode" value="whileScanning" />
            {{ t('carve.output.whileScanning') }}
          </label>
          <label class="carve-wizard__mode">
            <input v-model="saveMode" type="radio" name="carve-save-mode" value="afterScan" />
            {{ t('carve.output.afterScan') }}
          </label>
        </fieldset>
        <p v-if="saveMode === 'afterScan'" class="wizard__hint">{{ t('carve.output.afterScanHint') }}</p>
        <div v-if="saveMode === 'whileScanning'" class="wizard__path">
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
        <template v-if="saveMode === 'whileScanning'">
          <p class="wizard__hint">{{ t('carve.output.hint') }}</p>
          <p v-if="sourceKind === 'disk'" class="wizard__hint wizard__hint--warning">
            {{ t('carve.output.diskHint') }}
          </p>
        </template>
      </div>

      <div v-else class="wizard__panel">
        <dl class="wizard__details">
          <dt>{{ t('carve.run.source') }}</dt>
          <dd class="wizard__path-text">{{ sourceText }}</dd>
          <dt>{{ t('carve.run.formats') }}</dt>
          <dd>{{ selectedTypeNames }}</dd>
          <dt>{{ t('carve.run.output') }}</dt>
          <dd v-if="(status === 'idle' ? saveMode === 'afterScan' : runScanOnly)">
            {{ t('carve.run.outputAfterScan') }}
          </dd>
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
            <table class="wizard__table">
              <caption class="wizard__caption">
                {{ t('carve.run.found', { count: summary?.files ?? foundCount }) }}
              </caption>
              <thead>
                <tr>
                  <th scope="col">{{ t('carve.run.typeColumn') }}</th>
                  <th scope="col" class="wizard__number">{{ t('carve.run.countColumn') }}</th>
                  <th scope="col" class="wizard__number">{{ t('carve.run.sizeColumn') }}</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="row in typeRows" :key="row.extension">
                  <th scope="row">{{ row.name }}</th>
                  <td class="wizard__number">{{ row.totals.count }}</td>
                  <td class="wizard__number">{{ formatBytes(row.totals.bytes) }}</td>
                </tr>
              </tbody>
            </table>

            <div class="wizard__files">
              <RecoveredBrowser
                v-model:selected="selectedFile"
                v-model:checked="checkedNames"
                :files="files"
                :file-types="fileTypes"
                :device-size="total"
                :selectable="runScanOnly"
                @open="openFile"
              />
              <RecoveredPreview
                :job-id="jobId"
                :file="selectedFile"
                :type-name="selectedTypeName"
                :action-error="fileActionError"
                :scan-only="runScanOnly"
                :saved-job-id="selectedFile ? (savedBy.get(selectedFile.name) ?? null) : null"
                :can-open="!runScanOnly || (!isBusy && !save.isBusy.value)"
                @open="openFile"
                @reveal="revealFile"
              />
            </div>

            <div v-if="runScanOnly" class="wizard__panel">
              <div class="wizard__path">
                <label class="wizard__label" for="carve-save-dir">{{ t('carve.output.label') }}</label>
                <input
                  id="carve-save-dir"
                  v-model="outputDir"
                  @keydown.enter.prevent
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
              <p class="wizard__hint">{{ t('carve.save.hint') }}</p>
              <p v-if="targetKind === 'disk'" class="wizard__hint wizard__hint--warning">
                {{ t('carve.output.diskHint') }}
              </p>
              <div class="carve-wizard__save-actions">
                <button class="wizard__button" type="button" :disabled="!canSave" @click="handleSave">
                  {{ t('carve.save.start', { count: checkedFiles.length }) }}
                </button>
                <button
                  v-if="save.isBusy.value"
                  class="wizard__button wizard__button--secondary"
                  type="button"
                  :disabled="save.status.value !== 'running'"
                  @click="save.cancel"
                >
                  {{ save.status.value === 'cancelling' ? t('carve.run.cancelling') : t('carve.run.cancel') }}
                </button>
                <span class="wizard__note">
                  {{ t('carve.save.savedCount', { saved: savedBy.size, total: files.length }) }}
                </span>
              </div>
              <div v-if="save.status.value !== 'idle'" class="wizard__job" aria-live="polite">
                <progress
                  class="wizard__progress"
                  :value="save.done.value"
                  :max="save.total.value || 1"
                ></progress>
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
              </div>
            </div>
          </div>

          <dl v-if="summary && status === 'completed'" class="wizard__details">
            <dt>{{ t('carve.run.report.files') }}</dt>
            <dd>
              {{
                t('carve.run.report.filesValue', {
                  count: summary.files,
                  bytes: formatBytes(summary.bytes),
                })
              }}
            </dd>
            <dt>{{ t('carve.run.report.unreadable') }}</dt>
            <dd>{{ formatBytes(summary.unreadableBytes) }}</dd>
            <dt>{{ t('carve.run.report.elapsed') }}</dt>
            <dd>
              {{
                t('carve.run.report.elapsedValue', {
                  seconds: (summary.elapsedMs / 1000).toFixed(1),
                })
              }}
            </dd>
            <template v-if="summary.outputDir">
              <dt>{{ t('carve.run.report.outputDir') }}</dt>
              <dd class="wizard__path-text">{{ summary.outputDir }}</dd>
            </template>
          </dl>
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
          :disabled="isBusy || save.isBusy.value"
          @click="goBack"
        >
          {{ t('carve.previous') }}
        </button>
        <span class="wizard__nav-gap"></span>
        <button
          v-if="step !== 'run'"
          class="wizard__button"
          type="submit"
          :disabled="!canGoNext"
        >
          {{ t('carve.next') }}
        </button>
        <button v-else-if="!isBusy" class="wizard__button" type="submit" :disabled="!canStart">
          {{ status === 'idle' ? t('carve.run.start') : t('carve.run.restart') }}
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
    </form>
  </section>
</template>

<style scoped>
.carve-wizard__modes {
  display: flex;
  flex-direction: column;
  gap: var(--space-xs);
  margin: 0;
  padding: 0;
  border: none;
}

.carve-wizard__modes-legend {
  margin-bottom: var(--space-xs);
  padding: 0;
}

.carve-wizard__mode {
  display: flex;
  align-items: center;
  gap: var(--space-xs);
  cursor: pointer;
}

.carve-wizard__save-actions {
  display: flex;
  flex-wrap: wrap;
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
