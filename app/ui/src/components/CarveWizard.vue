<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import FileTypePicker from './FileTypePicker.vue'
import RecoveredBrowser from './RecoveredBrowser.vue'
import RecoveredPreview from './RecoveredPreview.vue'
import SourcePicker from './SourcePicker.vue'
import { useCarve } from '../composables/useCarve'
import { useFileTypeName } from '../composables/useFileTypeName'
import { useHostErrorText } from '../composables/useHostErrorText'
import {
  listFileTypes,
  openRecoveredFile,
  pickDirectory,
  revealRecoveredFile,
  toHostError,
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
const dialogError = ref<HostError | null>(null)
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
      return trimmedOutputDir.value !== ''
    case 'run':
      return false
  }
  return false
})

const canStart = computed(
  () =>
    target.value !== null &&
    selectedExtensions.value.length > 0 &&
    trimmedOutputDir.value !== '' &&
    !isBusy.value,
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

onMounted(() => {
  // 从首页切过来，焦点放到向导标题上，读屏软件从这里开始读
  titleElement.value?.focus()
  void loadFileTypes()
})

function handleExit() {
  if (!isBusy.value) {
    emit('exit')
  }
}

function goBack() {
  const previous = STEPS[stepIndex.value - 1]
  if (previous !== undefined && !isBusy.value) {
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
  if (!canStart.value || target.value === null) {
    return
  }
  runExtensions.value = [...selectedExtensions.value]
  selectedFile.value = null
  void start(target.value, trimmedOutputDir.value, runExtensions.value)
}

function handleCancel() {
  void cancel()
}

/** 结果列表里双击文件、或点「用默认程序打开」 */
async function openFile(file: FoundFile) {
  await runFileAction(file, openRecoveredFile)
}

async function revealFile(file: FoundFile) {
  await runFileAction(file, revealRecoveredFile)
}

async function runFileAction(
  file: FoundFile,
  action: (jobId: string, name: string) => Promise<void>,
) {
  if (jobId.value === null) {
    return
  }
  selectedFile.value = file
  fileActionError.value = null
  try {
    await action(jobId.value, file.name)
  } catch (error) {
    fileActionError.value = toHostError(error)
  }
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
      </div>

      <div v-else class="wizard__panel">
        <dl class="wizard__details">
          <dt>{{ t('carve.run.source') }}</dt>
          <dd class="wizard__path-text">{{ sourceText }}</dd>
          <dt>{{ t('carve.run.formats') }}</dt>
          <dd>{{ selectedTypeNames }}</dd>
          <dt>{{ t('carve.run.output') }}</dt>
          <dd class="wizard__path-text">{{ trimmedOutputDir }}</dd>
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
            {{ t('carve.run.cancelledNote') }}
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
                :files="files"
                :file-types="fileTypes"
                :device-size="total"
                @open="openFile"
              />
              <RecoveredPreview
                :job-id="jobId"
                :file="selectedFile"
                :type-name="selectedTypeName"
                :action-error="fileActionError"
                @open="openFile"
                @reveal="revealFile"
              />
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
            <dt>{{ t('carve.run.report.outputDir') }}</dt>
            <dd class="wizard__path-text">{{ summary.outputDir }}</dd>
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
          :disabled="isBusy"
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
