<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import DiskPicker from './DiskPicker.vue'
import FileTypePicker from './FileTypePicker.vue'
import { useCarve } from '../composables/useCarve'
import { useFileTypeName } from '../composables/useFileTypeName'
import { useHostErrorText } from '../composables/useHostErrorText'
import {
  listFileTypes,
  pickDirectory,
  pickFile,
  toHostError,
  type FileType,
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
  status,
  targetKind,
  done,
  total,
  percent,
  bytesPerSecond,
  foundCount,
  foundByType,
  recentFiles,
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
  void start(target.value, trimmedOutputDir.value, runExtensions.value)
}

function handleCancel() {
  void cancel()
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

async function browseImage() {
  await pickPath(() => pickFile(t('carve.source.pickTitle')), (path) => {
    imagePath.value = path
  })
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
  <section class="carve-wizard" aria-labelledby="carve-wizard-title">
    <header class="carve-wizard__header">
      <button
        class="carve-wizard__button carve-wizard__button--secondary"
        type="button"
        :disabled="isBusy"
        @click="handleExit"
      >
        {{ t('carve.exit') }}
      </button>
      <h2 id="carve-wizard-title" ref="titleElement" class="carve-wizard__title" tabindex="-1">
        {{ t('scenarios.carve') }}
      </h2>
    </header>

    <ol class="carve-wizard__steps">
      <li
        v-for="(name, index) in STEPS"
        :key="name"
        class="carve-wizard__step"
        :class="{
          'carve-wizard__step--current': name === step,
          'carve-wizard__step--done': index < stepIndex,
        }"
        :aria-current="name === step ? 'step' : undefined"
      >
        <span class="carve-wizard__step-number">{{ index + 1 }}</span>
        {{ t(`carve.steps.${name}`) }}
      </li>
    </ol>

    <form class="carve-wizard__form" @submit.prevent="handleSubmit">
      <div v-if="step === 'source'" class="carve-wizard__panel">
        <fieldset class="carve-wizard__choice">
          <legend class="carve-wizard__choice-legend">{{ t('carve.source.legend') }}</legend>
          <label class="carve-wizard__choice-option">
            <input v-model="sourceKind" type="radio" name="carve-source" value="image" />
            {{ t('carve.source.image') }}
          </label>
          <label class="carve-wizard__choice-option">
            <input v-model="sourceKind" type="radio" name="carve-source" value="disk" />
            {{ t('carve.source.disk') }}
          </label>
        </fieldset>

        <div v-if="sourceKind === 'image'" class="carve-wizard__path">
          <label class="carve-wizard__label" for="carve-image-path">
            {{ t('carve.source.imagePath') }}
          </label>
          <input
            id="carve-image-path"
            v-model="imagePath"
            class="carve-wizard__input"
            type="text"
            spellcheck="false"
            autocomplete="off"
            :placeholder="t('carve.source.imagePathPlaceholder')"
          />
          <button
            class="carve-wizard__button carve-wizard__button--secondary"
            type="button"
            @click="browseImage"
          >
            {{ t('carve.browseFile') }}
          </button>
        </div>
        <template v-else>
          <DiskPicker v-model="selectedDisk" :disabled="false" />
          <p class="carve-wizard__hint">{{ t('carve.source.diskHint') }}</p>
        </template>
      </div>

      <div v-else-if="step === 'formats'" class="carve-wizard__panel">
        <p v-if="isLoadingFileTypes" class="carve-wizard__note">{{ t('carve.formats.loading') }}</p>
        <div v-else-if="fileTypesError" class="carve-wizard__retry">
          <p class="carve-wizard__error" role="alert">{{ formatHostError(fileTypesError) }}</p>
          <button
            class="carve-wizard__button carve-wizard__button--secondary"
            type="button"
            @click="loadFileTypes"
          >
            {{ t('carve.formats.retry') }}
          </button>
        </div>
        <template v-else>
          <FileTypePicker v-model="selectedExtensions" :types="fileTypes" :disabled="false" />
          <p class="carve-wizard__hint">{{ t('carve.formats.hint') }}</p>
        </template>
      </div>

      <div v-else-if="step === 'output'" class="carve-wizard__panel">
        <div class="carve-wizard__path">
          <label class="carve-wizard__label" for="carve-output-dir">
            {{ t('carve.output.label') }}
          </label>
          <input
            id="carve-output-dir"
            v-model="outputDir"
            class="carve-wizard__input"
            type="text"
            spellcheck="false"
            autocomplete="off"
            :placeholder="t('carve.output.placeholder')"
          />
          <button
            class="carve-wizard__button carve-wizard__button--secondary"
            type="button"
            @click="browseOutputDir"
          >
            {{ t('carve.browseFolder') }}
          </button>
        </div>
        <p class="carve-wizard__hint">{{ t('carve.output.hint') }}</p>
        <p v-if="sourceKind === 'disk'" class="carve-wizard__hint carve-wizard__hint--warning">
          {{ t('carve.output.diskHint') }}
        </p>
      </div>

      <div v-else class="carve-wizard__panel">
        <dl class="carve-wizard__details">
          <dt>{{ t('carve.run.source') }}</dt>
          <dd class="carve-wizard__path-text">{{ sourceText }}</dd>
          <dt>{{ t('carve.run.formats') }}</dt>
          <dd>{{ selectedTypeNames }}</dd>
          <dt>{{ t('carve.run.output') }}</dt>
          <dd class="carve-wizard__path-text">{{ trimmedOutputDir }}</dd>
        </dl>

        <div v-if="status !== 'idle'" class="carve-wizard__job" aria-live="polite">
          <progress class="carve-wizard__progress" :value="done" :max="total || 1"></progress>
          <p class="carve-wizard__status">{{ statusText }}</p>
          <p v-if="isUnresponsive" class="carve-wizard__error" role="alert">
            {{ t('carve.run.unresponsive') }}
          </p>
          <p v-if="failure" class="carve-wizard__error" role="alert">
            {{ formatHostError(failure) }}
          </p>
          <p v-if="status === 'cancelled'" class="carve-wizard__hint">
            {{ t('carve.run.cancelledNote') }}
          </p>

          <div v-if="hasResults" class="carve-wizard__results">
            <table class="carve-wizard__table">
              <caption class="carve-wizard__caption">
                {{ t('carve.run.found', { count: summary?.files ?? foundCount }) }}
              </caption>
              <thead>
                <tr>
                  <th scope="col">{{ t('carve.run.typeColumn') }}</th>
                  <th scope="col" class="carve-wizard__number">{{ t('carve.run.countColumn') }}</th>
                  <th scope="col" class="carve-wizard__number">{{ t('carve.run.sizeColumn') }}</th>
                </tr>
              </thead>
              <tbody>
                <tr v-for="row in typeRows" :key="row.extension">
                  <th scope="row">{{ row.name }}</th>
                  <td class="carve-wizard__number">{{ row.totals.count }}</td>
                  <td class="carve-wizard__number">{{ formatBytes(row.totals.bytes) }}</td>
                </tr>
              </tbody>
            </table>

            <div v-if="recentFiles.length > 0" class="carve-wizard__recent">
              <h3 class="carve-wizard__subtitle">{{ t('carve.run.recent') }}</h3>
              <ul class="carve-wizard__recent-list">
                <li v-for="file in recentFiles" :key="file.name">
                  {{ file.name }}
                  <span class="carve-wizard__muted">{{ formatBytes(file.length) }}</span>
                </li>
              </ul>
            </div>
          </div>

          <dl v-if="summary && status === 'completed'" class="carve-wizard__details">
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
            <dd class="carve-wizard__path-text">{{ summary.outputDir }}</dd>
          </dl>
        </div>
      </div>

      <p v-if="dialogError" class="carve-wizard__error" role="alert">
        {{ formatHostError(dialogError) }}
      </p>

      <div class="carve-wizard__nav">
        <button
          v-if="stepIndex > 0"
          class="carve-wizard__button carve-wizard__button--secondary"
          type="button"
          :disabled="isBusy"
          @click="goBack"
        >
          {{ t('carve.previous') }}
        </button>
        <span class="carve-wizard__nav-gap"></span>
        <button
          v-if="step !== 'run'"
          class="carve-wizard__button"
          type="submit"
          :disabled="!canGoNext"
        >
          {{ t('carve.next') }}
        </button>
        <button v-else-if="!isBusy" class="carve-wizard__button" type="submit" :disabled="!canStart">
          {{ status === 'idle' ? t('carve.run.start') : t('carve.run.restart') }}
        </button>
        <button
          v-else
          class="carve-wizard__button carve-wizard__button--secondary"
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
.carve-wizard {
  display: block;
  width: 100%;
  padding: var(--space-md);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-card);
}

.carve-wizard__header {
  display: flex;
  align-items: center;
  gap: var(--space-md);
}

.carve-wizard__title {
  margin: 0;
  font-size: var(--font-size-section-title);
  font-weight: 600;
}

/* 标题只是切换视图后的焦点落点，不可操作，不画焦点框 */
.carve-wizard__title:focus-visible {
  outline: none;
}

.carve-wizard__steps {
  display: flex;
  gap: var(--space-lg);
  margin: var(--space-md) 0;
  padding: 0 0 var(--space-sm);
  list-style: none;
  border-bottom: 1px solid var(--color-border);
}

.carve-wizard__step {
  display: flex;
  align-items: center;
  gap: var(--space-xs);
  color: var(--color-text-muted);
}

.carve-wizard__step--current {
  color: var(--color-text);
  font-weight: 600;
}

.carve-wizard__step-number {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 20px;
  height: 20px;
  font-size: var(--font-size-small);
  border: 1px solid var(--color-border);
  border-radius: 50%;
}

.carve-wizard__step--current .carve-wizard__step-number {
  color: var(--color-on-accent);
  background: var(--color-accent);
  border-color: var(--color-accent);
}

.carve-wizard__step--done .carve-wizard__step-number {
  color: var(--color-accent);
  border-color: var(--color-accent);
}

.carve-wizard__form {
  display: flex;
  flex-direction: column;
  gap: var(--space-md);
}

.carve-wizard__panel {
  display: flex;
  flex-direction: column;
  gap: var(--space-sm);
}

.carve-wizard__choice {
  display: flex;
  gap: var(--space-md);
  margin: 0;
  padding: 0;
  border: none;
}

.carve-wizard__choice-legend {
  float: left;
  margin-right: var(--space-sm);
  padding: 0;
}

.carve-wizard__choice-option {
  display: flex;
  align-items: center;
  gap: var(--space-xs);
  cursor: pointer;
}

.carve-wizard__path {
  display: flex;
  align-items: center;
  gap: var(--space-sm);
}

.carve-wizard__label {
  flex: none;
}

.carve-wizard__input {
  flex: 1;
  min-width: 0;
  padding: var(--space-xs) var(--space-sm);
  color: var(--color-text);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--space-xs);
  font: inherit;
}

.carve-wizard__hint,
.carve-wizard__note {
  margin: 0;
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
}

.carve-wizard__hint--warning {
  color: var(--color-text);
  font-weight: 600;
}

.carve-wizard__retry {
  display: flex;
  align-items: center;
  gap: var(--space-sm);
}

.carve-wizard__error {
  margin: 0;
  color: var(--color-danger);
}

.carve-wizard__details {
  display: grid;
  grid-template-columns: max-content 1fr;
  gap: var(--space-xs) var(--space-md);
  margin: 0;
}

.carve-wizard__details dt {
  color: var(--color-text-muted);
}

.carve-wizard__details dd {
  margin: 0;
}

.carve-wizard__path-text {
  overflow-wrap: anywhere;
  user-select: text;
}

.carve-wizard__job {
  display: flex;
  flex-direction: column;
  gap: var(--space-sm);
}

.carve-wizard__progress {
  display: block;
  width: 100%;
  height: var(--space-sm);
  accent-color: var(--color-accent);
}

.carve-wizard__status {
  margin: 0;
}

.carve-wizard__results {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
  gap: var(--space-lg);
  align-items: start;
}

.carve-wizard__table {
  width: 100%;
  border-collapse: collapse;
}

.carve-wizard__caption {
  padding-bottom: var(--space-xs);
  font-weight: 600;
  text-align: left;
}

.carve-wizard__table th,
.carve-wizard__table td {
  padding: var(--space-xs) var(--space-sm);
  text-align: left;
  border-bottom: 1px solid var(--color-border);
}

.carve-wizard__table thead th {
  color: var(--color-text-muted);
  font-weight: normal;
}

.carve-wizard__table tbody th {
  font-weight: normal;
}

.carve-wizard__table .carve-wizard__number {
  text-align: right;
  font-variant-numeric: tabular-nums;
}

.carve-wizard__subtitle {
  margin: 0 0 var(--space-xs);
  font-size: var(--font-size-body);
  font-weight: 600;
}

.carve-wizard__recent-list {
  margin: 0;
  padding: 0;
  list-style: none;
  font-variant-numeric: tabular-nums;
}

.carve-wizard__muted {
  margin-left: var(--space-sm);
  color: var(--color-text-muted);
}

.carve-wizard__nav {
  display: flex;
  align-items: center;
  gap: var(--space-sm);
}

.carve-wizard__nav-gap {
  flex: 1;
}

.carve-wizard__button {
  flex: none;
  padding: var(--space-xs) var(--space-md);
  color: var(--color-on-accent);
  background: var(--color-accent);
  border: 1px solid var(--color-accent);
  border-radius: var(--space-xs);
  font: inherit;
  cursor: pointer;
}

.carve-wizard__button--secondary {
  color: var(--color-text);
  background: var(--color-surface);
  border-color: var(--color-border);
}

.carve-wizard__button:disabled {
  opacity: 0.5;
  cursor: default;
}
</style>
