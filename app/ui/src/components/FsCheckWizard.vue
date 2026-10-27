<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import PartitionPicker from './PartitionPicker.vue'
import SourcePicker from './SourcePicker.vue'
import { useBrowse } from '../composables/useBrowse'
import { useFsCheck } from '../composables/useFsCheck'
import { useHostErrorText } from '../composables/useHostErrorText'
import { usePartitionText } from '../composables/usePartitionText'
import {
  pickSaveFile,
  saveFsCheckReport,
  toHostError,
  type FsIssue,
  type FsIssueSeverity,
  type HostError,
  type JobTarget,
  type PartitionInfo,
} from '../services/host'
import { formatBytes } from '../utils/format'

const STEPS = ['source', 'partition', 'check', 'result'] as const
type WizardStep = (typeof STEPS)[number]
const SEVERITIES: FsIssueSeverity[] = ['dataLoss', 'structure', 'minor']
/** 能检查的文件系统；认不出的也可以查：检查会试着读备份引导扇区 */
const CHECKABLE = ['fat12', 'fat16', 'fat32', 'exfat', 'ntfs', 'unknown', 'unreadable']
/** 期望值、实际值是字节数或校验和的问题，换成好读的写法 */
const BYTE_VALUES = ['volumeTruncated', 'bitmapInvalid']
const HEX_VALUES = ['entrySetChecksum', 'upcaseChecksum', 'fatMediaMismatch']

const emit = defineEmits<{ exit: [] }>()

const { t, te, locale } = useI18n()
const { formatHostError } = useHostErrorText()
const { filesystemName, sourceName } = usePartitionText()
const browse = useBrowse()
const fsCheck = useFsCheck()

const titleElement = ref<HTMLHeadingElement | null>(null)
const step = ref<WizardStep>('source')
const sourceKind = ref<JobTarget['kind']>('image')
const imagePath = ref('')
const selectedDisk = ref<number | null>(null)
const selectedPartition = ref<number | null>(null)
const exportError = ref<HostError | null>(null)
const exportedPath = ref<string | null>(null)
const isExporting = ref(false)
/** 检查完成的时刻，写进导出的清单 */
const finishedAt = ref<Date | null>(null)

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
const report = computed(() => fsCheck.report.value)
const checkedPartition = computed(() => fsCheck.partition.value)

const canGoNext = computed(() => {
  switch (step.value) {
    case 'source':
      return target.value !== null && !browse.isTransitioning.value
    case 'partition':
      return currentPartition.value !== null && isCheckable(currentPartition.value) && browse.isOpen.value
    default:
      return false
  }
})
const canGoBack = computed(
  () => stepIndex.value > 0 && !fsCheck.isBusy.value && !browse.isTransitioning.value,
)
const canRecheck = computed(
  () =>
    (step.value === 'check' || step.value === 'result') &&
    !fsCheck.isBusy.value &&
    fsCheck.target.value !== null &&
    fsCheck.partition.value !== null,
)

const sourceText = computed(() => sourceName(browse.target.value ?? fsCheck.target.value))

const checkStatusText = computed(() => {
  const stage = fsCheck.stage.value
  const stageName = stage !== null && te(`fsCheck.stages.${stage}`) ? t(`fsCheck.stages.${stage}`) : ''
  switch (fsCheck.status.value) {
    case 'starting':
      return fsCheck.target.value?.kind === 'disk' ? t('fsCheck.run.startingDisk') : t('fsCheck.run.starting')
    case 'running':
      if (fsCheck.isPaused.value) {
        return t('fsCheck.run.paused', { stage: stageName })
      }
      return stageName === ''
        ? t('fsCheck.run.starting')
        : t('fsCheck.run.running', { stage: stageName, percent: fsCheck.percent.value })
    case 'cancelling':
      return t('carve.run.cancelling')
    case 'cancelled':
      return t('fsCheck.run.cancelled')
    case 'failed':
      return t('fsCheck.run.failed')
  }
  return ''
})

/** 按严重程度分组；没有问题的组不显示 */
const groups = computed(() => {
  const issues = report.value?.issues ?? []
  return SEVERITIES.map((severity) => ({
    severity,
    total: report.value?.totals[severity] ?? 0,
    issues: issues.filter((issue) => issue.severity === severity),
  })).filter((group) => group.total > 0)
})

const totalIssues = computed(() =>
  SEVERITIES.reduce((sum, severity) => sum + (report.value?.totals[severity] ?? 0), 0),
)

watch(fsCheck.status, (status) => {
  if (status === 'completed') {
    finishedAt.value = new Date()
    step.value = 'result'
  }
})

onMounted(() => {
  titleElement.value?.focus()
})

async function handleExit() {
  if (fsCheck.isBusy.value) {
    return
  }
  if (browse.isOpen.value) {
    await browse.close()
  }
  emit('exit')
}

async function handleSubmit() {
  if (step.value === 'source') {
    await openSource()
  } else if (step.value === 'partition' && canGoNext.value) {
    await startCheck()
  }
}

async function openSource() {
  const current = target.value
  if (current === null || !canGoNext.value) {
    return
  }
  await browse.open(current)
  if (browse.isOpen.value) {
    if (!partitions.value.some((partition) => partition.number === selectedPartition.value)) {
      selectedPartition.value = partitions.value.find(isCheckable)?.number ?? null
    }
    step.value = 'partition'
  }
}

/** 先关掉浏览会话（不再占着源），再开始检查 */
async function startCheck() {
  const source = browse.target.value
  const partition = currentPartition.value
  if (source === null || partition === null) {
    return
  }
  await browse.close()
  resetExport()
  step.value = 'check'
  await fsCheck.start(source, partition)
}

async function recheck() {
  const source = fsCheck.target.value
  const partition = fsCheck.partition.value
  if (!canRecheck.value || source === null || partition === null) {
    return
  }
  resetExport()
  step.value = 'check'
  await fsCheck.start(source, partition)
}

/** 回到分区这一步要重新读分区表；回到来源这一步时关闭浏览 */
async function goBack() {
  if (!canGoBack.value) {
    return
  }
  if (step.value === 'partition') {
    await browse.close()
    step.value = 'source'
    return
  }
  const source = fsCheck.target.value ?? browse.target.value
  if (source === null) {
    step.value = 'source'
    return
  }
  await browse.open(source)
  step.value = browse.isOpen.value ? 'partition' : 'source'
}

function resetExport() {
  exportError.value = null
  exportedPath.value = null
}

/** 另存为：问题清单按当前语言生成文本，存到用户选的 .txt 文件 */
async function handleExport() {
  const source = fsCheck.target.value
  if (report.value === null || source === null || isExporting.value) {
    return
  }
  resetExport()
  isExporting.value = true
  try {
    const path = await pickSaveFile(t('fsCheck.export.title'), defaultFileName(), {
      name: t('fsCheck.export.fileType'),
      extension: 'txt',
    })
    if (path === null) {
      return
    }
    await saveFsCheckReport(source, path, buildReportText())
    exportedPath.value = path
  } catch (error) {
    exportError.value = toHostError(error)
  } finally {
    isExporting.value = false
  }
}

function isCheckable(partition: PartitionInfo): boolean {
  return CHECKABLE.includes(partition.filesystem)
}

/** 分区这一步每行的附注：不能检查的原因，或者这种文件系统查得到哪些 */
function partitionNote(partition: PartitionInfo): string {
  const key = `fsCheck.partition.note.${partition.filesystem}`
  if (te(key)) {
    return t(key)
  }
  return isCheckable(partition) ? '' : t('fsCheck.partition.note.other')
}

function partitionText(partition: PartitionInfo): string {
  const label = [partition.label, partition.name].find((text) => text !== null && text !== '')
  return t('fsCheck.result.partitionValue', {
    number: partition.number,
    // 没有卷标时不留多余的空格
    label: label === undefined ? '' : ` ${label}`,
    filesystem: filesystemName(partition.filesystem),
    size: formatBytes(partition.length),
  })
}

function number(value: number | null): string {
  return value === null ? '' : value.toLocaleString(locale.value)
}

function hex(value: number | null): string {
  return value === null ? '' : `0x${value.toString(16).toUpperCase()}`
}

/** `$Bitmap`、`$UpCase` 是 exFAT 的系统结构，换成本地化的名字 */
function displayPath(path: string | null): string {
  if (path === null) {
    return ''
  }
  const key = `fsCheck.system.${path.replace('$', '')}`
  return path.startsWith('$') && te(key) ? t(key) : path
}

function issueParams(issue: FsIssue): Record<string, string> {
  const valueText = (value: number | null) => {
    if (BYTE_VALUES.includes(issue.kind)) {
      return value === null ? '' : formatBytes(value)
    }
    return HEX_VALUES.includes(issue.kind) ? hex(value) : number(value)
  }
  const areaKey = `fsCheck.areas.${issue.area ?? ''}`
  const fieldKey = `fsCheck.fields.${issue.field ?? ''}`
  return {
    path: displayPath(issue.path),
    otherPath: displayPath(issue.otherPath),
    cluster: number(issue.cluster),
    count: number(issue.count),
    bytes: issue.count === null ? '' : formatBytes(issue.count),
    offset: number(issue.offset),
    expected: valueText(issue.expected),
    actual: valueText(issue.actual),
    area: issue.area !== null && te(areaKey) ? t(areaKey) : (issue.area ?? ''),
    field: issue.field !== null && te(fieldKey) ? t(fieldKey) : (issue.field ?? ''),
  }
}

/** 一个问题的标题、说明与以后的修法；不认识的类型给一句通用的话 */
function issueText(issue: FsIssue, part: 'title' | 'detail' | 'fix'): string {
  const key = `fsCheck.issue.${issue.kind}.${part}`
  const params = issueParams(issue)
  if (!te(key)) {
    return part === 'title' ? t('fsCheck.issue.fallback', { kind: issue.kind }) : ''
  }
  return t(key, params, issue.count ?? issue.actual ?? 1)
}

/** 展开后列出的位置信息：只列这个问题用到的 */
function issueFacts(issue: FsIssue): { label: string; value: string }[] {
  const params = issueParams(issue)
  const facts: { label: string; value: string }[] = []
  if (issue.path !== null) {
    facts.push({ label: t('fsCheck.facts.path'), value: params.path })
  }
  if (issue.otherPath !== null) {
    facts.push({ label: t('fsCheck.facts.otherPath'), value: params.otherPath })
  }
  if (issue.area !== null) {
    facts.push({ label: t('fsCheck.facts.area'), value: params.area })
  }
  if (issue.cluster !== null) {
    facts.push({ label: t('fsCheck.facts.cluster'), value: params.cluster })
  }
  if (issue.offset !== null) {
    facts.push({ label: t('fsCheck.facts.offset'), value: t('fsCheck.facts.offsetValue', params) })
  }
  return facts
}

function pad(value: number): string {
  return String(value).padStart(2, '0')
}

function formatTime(date: Date): string {
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`
}

function defaultFileName(): string {
  const date = finishedAt.value ?? new Date()
  const stamp = `${date.getFullYear()}${pad(date.getMonth() + 1)}${pad(date.getDate())}-${pad(date.getHours())}${pad(date.getMinutes())}${pad(date.getSeconds())}`
  return `fs-check-${checkedPartition.value?.number ?? 0}-${stamp}.txt`
}

function summaryText(): string {
  const current = report.value
  if (current === null) {
    return ''
  }
  return t('fsCheck.result.volumeValue', {
    filesystem: filesystemName(current.filesystem),
    clusterSize: formatBytes(current.clusterSize),
    clusters: number(current.clusterCount),
    files: number(current.files),
    directories: number(current.directories),
  })
}

function totalsText(): string {
  const current = report.value
  if (current === null) {
    return ''
  }
  if (totalIssues.value === 0) {
    return t('fsCheck.result.clean')
  }
  return t('fsCheck.result.totalsValue', {
    dataLoss: number(current.totals.dataLoss),
    structure: number(current.totals.structure),
    minor: number(current.totals.minor),
  })
}

function checksText(keys: string[]): string {
  return keys
    .map((key) => (te(`fsCheck.checks.${key}`) ? t(`fsCheck.checks.${key}`) : key))
    .join(t('fsCheck.result.listSeparator'))
}

function groupTitle(severity: FsIssueSeverity, total: number): string {
  return t('fsCheck.result.groupTitle', { name: t(`fsCheck.severity.${severity}`), count: number(total) })
}

/** 导出的问题清单：概况、各组问题（标题、说明、位置、以后的修法）、说明 */
function buildReportText(): string {
  const current = report.value
  const partition = checkedPartition.value
  if (current === null || partition === null) {
    return ''
  }
  const field = (label: string, value: string) => t('fsCheck.labelValue', { label, value })
  const elapsed = t('fsCheck.result.elapsed', { seconds: (current.elapsedMs / 1000).toFixed(1) })
  const lines = [
    t('fsCheck.export.heading'),
    '',
    field(t('fsCheck.result.source'), sourceName(fsCheck.target.value)),
    field(t('fsCheck.result.partition'), partitionText(partition)),
    field(t('fsCheck.result.finishedAt'), formatTime(finishedAt.value ?? new Date())),
    field(t('fsCheck.result.volume'), summaryText()),
    field(t('fsCheck.result.totals'), `${totalsText()} ${elapsed}`),
    field(t('fsCheck.result.checked'), checksText(current.checked)),
  ]
  if (current.skipped.length > 0) {
    lines.push(field(t('fsCheck.result.skipped'), checksText(current.skipped)))
  }
  if (current.unreadableBytes > 0) {
    lines.push(t('fsCheck.result.unreadable', { bytes: formatBytes(current.unreadableBytes) }))
  }
  for (const group of groups.value) {
    lines.push('', `== ${groupTitle(group.severity, group.total)} ==`)
    group.issues.forEach((issue, index) => {
      lines.push(`${index + 1}. ${issueText(issue, 'title')}`)
      const detail = issueText(issue, 'detail')
      if (detail !== '') {
        lines.push(`   ${field(t('fsCheck.facts.detail'), detail)}`)
      }
      for (const fact of issueFacts(issue)) {
        lines.push(`   ${field(fact.label, fact.value)}`)
      }
      const fix = issueText(issue, 'fix')
      if (fix !== '') {
        lines.push(`   ${field(t('fsCheck.facts.fix'), fix)}`)
      }
    })
  }
  if (current.omittedIssues > 0) {
    lines.push('', t('fsCheck.result.omitted', { count: number(current.omittedIssues) }, current.omittedIssues))
  }
  lines.push('', t('fsCheck.notes.readOnly'), t('fsCheck.notes.imageFirst'))
  return `${lines.join('\n')}\n`
}
</script>

<template>
  <section
    class="wizard fs-check"
    :class="{ 'wizard--fill': step === 'result' }"
    aria-labelledby="fs-check-title"
  >
    <header class="wizard__header">
      <button
        class="wizard__button wizard__button--secondary"
        type="button"
        :disabled="fsCheck.isBusy.value"
        @click="handleExit"
      >
        {{ t('nav.back') }}
      </button>
      <h2 id="fs-check-title" ref="titleElement" class="wizard__title" tabindex="-1">
        {{ t('features.fsCheck.name') }}
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
            {{ t(`fsCheck.steps.${name}`) }}
          </li>
        </ol>
        <div class="wizard__nav">
          <button
            v-if="stepIndex > 0 && !fsCheck.isBusy.value"
            class="wizard__button wizard__button--secondary"
            type="button"
            :disabled="!canGoBack"
            @click="goBack"
          >
            {{ t('carve.previous') }}
          </button>
          <button v-if="step === 'source'" class="wizard__button" type="submit" :disabled="!canGoNext">
            {{ t('carve.next') }}
          </button>
          <button v-else-if="step === 'partition'" class="wizard__button" type="submit" :disabled="!canGoNext">
            {{ t('fsCheck.partition.start') }}
          </button>
          <template v-else-if="fsCheck.isBusy.value">
            <button
              v-if="fsCheck.status.value === 'running'"
              class="wizard__button wizard__button--secondary"
              type="button"
              :disabled="fsCheck.isTogglingPause.value"
              @click="fsCheck.isPaused.value ? fsCheck.resume() : fsCheck.pause()"
            >
              {{ fsCheck.isPaused.value ? t('carve.run.resume') : t('carve.run.pause') }}
            </button>
            <button
              class="wizard__button wizard__button--secondary"
              type="button"
              :disabled="fsCheck.status.value !== 'running'"
              @click="fsCheck.cancel"
            >
              {{ fsCheck.status.value === 'cancelling' ? t('carve.run.cancelling') : t('carve.run.cancel') }}
            </button>
          </template>
          <template v-else>
            <button
              v-if="step === 'result'"
              class="wizard__button"
              type="button"
              :disabled="isExporting || report === null"
              @click="handleExport"
            >
              {{ t('fsCheck.export.button') }}
            </button>
            <button
              class="wizard__button wizard__button--secondary"
              type="button"
              :disabled="!canRecheck"
              @click="recheck"
            >
              {{ t('fsCheck.run.restart') }}
            </button>
          </template>
        </div>
      </div>

      <div v-if="step === 'source'" class="wizard__panel">
        <p class="wizard__note">{{ t('fsCheck.intro') }}</p>
        <SourcePicker
          v-model:kind="sourceKind"
          v-model:image-path="imagePath"
          v-model:disk="selectedDisk"
          :disk-hint="t('fsCheck.diskHint')"
        />
        <p class="wizard__hint wizard__hint--warning">{{ t('fsCheck.notes.imageFirst') }}</p>
        <p class="wizard__hint">{{ t('fsCheck.notes.readOnly') }}</p>
        <p v-if="browse.status.value === 'starting'" class="wizard__note" aria-live="polite">
          {{ target?.kind === 'disk' ? t('undelete.openingDisk') : t('undelete.opening') }}
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
        <PartitionPicker
          v-model="selectedPartition"
          :partitions="partitions"
          :caption="t('fsCheck.partition.caption')"
          :is-selectable="isCheckable"
          :note="partitionNote"
        />
        <p v-if="!partitions.some(isCheckable)" class="wizard__note">{{ t('fsCheck.partition.none') }}</p>
        <p class="wizard__hint">{{ t('fsCheck.partition.hint') }}</p>
        <p v-if="browse.failure.value" class="wizard__error" role="alert">
          {{ formatHostError(browse.failure.value) }}
        </p>
      </div>

      <div v-else-if="step === 'check'" class="wizard__panel">
        <dl v-if="checkedPartition" class="wizard__details">
          <dt>{{ t('fsCheck.result.source') }}</dt>
          <dd class="wizard__path-text">{{ sourceName(fsCheck.target.value) }}</dd>
          <dt>{{ t('fsCheck.result.partition') }}</dt>
          <dd>{{ partitionText(checkedPartition) }}</dd>
        </dl>
        <div class="wizard__job" aria-live="polite">
          <progress
            class="wizard__progress"
            :value="fsCheck.done.value"
            :max="fsCheck.total.value || 1"
          ></progress>
          <p class="wizard__status">{{ checkStatusText }}</p>
        </div>
        <p v-if="fsCheck.isPaused.value" class="wizard__hint">{{ t('fsCheck.run.pausedHint') }}</p>
        <p v-if="fsCheck.isUnresponsive.value" class="wizard__error" role="alert">
          {{ t('carve.run.unresponsive') }}
        </p>
        <p v-if="fsCheck.failure.value" class="wizard__error" role="alert">
          {{ formatHostError(fsCheck.failure.value) }}
        </p>
        <p class="wizard__hint">{{ t('fsCheck.notes.readOnly') }}</p>
      </div>

      <div v-else-if="report && checkedPartition" class="wizard__panel wizard__fill fs-check__result">
        <dl class="wizard__details">
          <dt>{{ t('fsCheck.result.source') }}</dt>
          <dd class="wizard__path-text">{{ sourceName(fsCheck.target.value) }}</dd>
          <dt>{{ t('fsCheck.result.partition') }}</dt>
          <dd>{{ partitionText(checkedPartition) }}</dd>
          <dt>{{ t('fsCheck.result.volume') }}</dt>
          <dd>{{ summaryText() }}</dd>
          <dt>{{ t('fsCheck.result.totals') }}</dt>
          <dd>
            {{ totalsText() }}
            {{ t('fsCheck.result.elapsed', { seconds: (report.elapsedMs / 1000).toFixed(1) }) }}
          </dd>
          <dt>{{ t('fsCheck.result.checked') }}</dt>
          <dd>{{ checksText(report.checked) }}</dd>
          <template v-if="report.skipped.length > 0">
            <dt>{{ t('fsCheck.result.skipped') }}</dt>
            <dd>{{ checksText(report.skipped) }}</dd>
          </template>
        </dl>
        <p v-if="report.unreadableBytes > 0" class="wizard__hint wizard__hint--warning">
          {{ t('fsCheck.result.unreadable', { bytes: formatBytes(report.unreadableBytes) }) }}
        </p>

        <div class="fs-check__issues wizard__fill" role="region" :aria-label="t('fsCheck.result.issues')">
          <p v-if="totalIssues === 0" class="fs-check__clean">{{ t('fsCheck.result.cleanDetail') }}</p>
          <section v-for="group in groups" :key="group.severity" class="fs-check__group">
            <h3 class="fs-check__group-title" :class="`fs-check__group-title--${group.severity}`">
              {{ groupTitle(group.severity, group.total) }}
              <span class="fs-check__group-hint">{{ t(`fsCheck.severityHint.${group.severity}`) }}</span>
            </h3>
            <ul class="fs-check__list">
              <li v-for="(issue, index) in group.issues" :key="index" class="fs-check__issue">
                <details>
                  <summary class="fs-check__summary">{{ issueText(issue, 'title') }}</summary>
                  <div class="fs-check__body">
                    <p v-if="issueText(issue, 'detail')" class="fs-check__text">{{ issueText(issue, 'detail') }}</p>
                    <dl v-if="issueFacts(issue).length > 0" class="wizard__details">
                      <template v-for="fact in issueFacts(issue)" :key="fact.label">
                        <dt>{{ fact.label }}</dt>
                        <dd class="wizard__path-text">{{ fact.value }}</dd>
                      </template>
                    </dl>
                    <p v-if="issueText(issue, 'fix')" class="fs-check__text">
                      <strong>{{ t('fsCheck.labelValue', { label: t('fsCheck.facts.fix'), value: '' }) }}</strong>{{ issueText(issue, 'fix') }}
                    </p>
                  </div>
                </details>
              </li>
            </ul>
          </section>
          <p v-if="report.omittedIssues > 0" class="wizard__note">
            {{ t('fsCheck.result.omitted', { count: number(report.omittedIssues) }, report.omittedIssues) }}
          </p>
        </div>

        <p class="wizard__hint wizard__hint--warning">{{ t('fsCheck.notes.readOnly') }}</p>
        <p class="wizard__hint">{{ t('fsCheck.notes.imageFirst') }}</p>
        <p v-if="exportedPath" class="wizard__note" aria-live="polite">
          {{ t('fsCheck.export.saved', { path: exportedPath }) }}
        </p>
        <p v-if="exportError" class="wizard__error" role="alert">{{ formatHostError(exportError) }}</p>
      </div>
    </form>
  </section>
</template>

<style scoped>

.fs-check__result {
  display: flex;
  flex-direction: column;
}

/* 问题清单不按内容撑高，在框里滚动；至少约 10 行高 */
.fs-check__issues {
  display: flex;
  flex-direction: column;
  gap: var(--space-md);
  min-height: 280px;
  overflow-y: auto;
  padding: var(--space-sm) var(--space-md);
  border: 1px solid var(--color-border);
  border-radius: var(--space-xs);
  contain: size;
}

.fs-check__clean {
  margin: 0;
}

.fs-check__group-title {
  margin: 0 0 var(--space-xs);
  font-size: var(--font-size-body);
  font-weight: 600;
}

/* 严重程度除了颜色还有文字，不只靠颜色区分 */
.fs-check__group-title--dataLoss {
  color: var(--color-danger);
}

.fs-check__group-hint {
  margin-left: var(--space-sm);
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
  font-weight: normal;
}

.fs-check__list {
  display: flex;
  flex-direction: column;
  gap: var(--space-xs);
  margin: 0;
  padding: 0;
  list-style: none;
}

.fs-check__issue {
  border-bottom: 1px solid var(--color-border);
}

.fs-check__summary {
  padding: var(--space-xs) 0;
  overflow-wrap: anywhere;
  cursor: pointer;
}

.fs-check__body {
  display: flex;
  flex-direction: column;
  gap: var(--space-xs);
  padding: 0 0 var(--space-sm) var(--space-lg);
}

.fs-check__text {
  margin: 0;
}
</style>
