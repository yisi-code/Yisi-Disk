<script setup lang="ts">
import { computed, onMounted, ref, shallowRef } from 'vue'
import { useI18n } from 'vue-i18n'
import PartitionFiles from './PartitionFiles.vue'
import SourcePicker from './SourcePicker.vue'
import { useBrowse } from '../composables/useBrowse'
import { useHostErrorText } from '../composables/useHostErrorText'
import {
  diagnoseBoot,
  toHostError,
  type BootCopyReport,
  type BootDiagnosis,
  type BootDifference,
  type BootIssue,
  type HostError,
  type JobTarget,
  type PartitionBoot,
  type PartitionRef,
} from '../services/host'
import { formatBytes } from '../utils/format'

const STEPS = ['source', 'diagnose', 'result'] as const
type WizardStep = (typeof STEPS)[number]
/** 这些字段按十六进制显示 */
const HEX_FIELDS = ['volumeSerial', 'media']

const emit = defineEmits<{ exit: [] }>()

const { t, te, locale } = useI18n()
const { formatHostError } = useHostErrorText()
const browse = useBrowse()

const titleElement = ref<HTMLHeadingElement | null>(null)
const step = ref<WizardStep>('source')
const sourceKind = ref<JobTarget['kind']>('image')
const imagePath = ref('')
const selectedDisk = ref<number | null>(null)
const diagnosis = shallowRef<BootDiagnosis | null>(null)
/** 诊断时的源：用备份浏览时打开同一个源 */
const diagnosedTarget = shallowRef<JobTarget | null>(null)
const isDiagnosing = ref(false)
const diagnoseFailure = ref<HostError | null>(null)
/** 正在用备份浏览的分区；为 null 时结果页列出诊断结论 */
const browsingNumber = ref<number | null>(null)
/** 正在为哪个分区打开浏览会话 */
const openingNumber = ref<number | null>(null)
const browseError = ref<{ number: number; text: string } | null>(null)
const isCopying = ref(false)

const stepIndex = computed(() => STEPS.indexOf(step.value))

const target = computed<JobTarget | null>(() => {
  if (sourceKind.value === 'image') {
    const path = imagePath.value.trim()
    return path === '' ? null : { kind: 'image', path }
  }
  return selectedDisk.value === null ? null : { kind: 'disk', diskNumber: selectedDisk.value }
})

/** 用备份浏览时，浏览会话里这个分区的信息 */
const browsingPartition = computed(() =>
  browsingNumber.value === null
    ? null
    : (browse.opened.value?.partitions.find((partition) => partition.number === browsingNumber.value) ??
      null),
)
const isBrowsing = computed(() => step.value === 'result' && browsingPartition.value !== null)
const browsingRef = computed<PartitionRef | null>(() => {
  const partition = browsingPartition.value
  return partition === null
    ? null
    : { number: partition.number, offset: partition.offset, length: partition.length }
})

const isBusy = computed(
  () => isDiagnosing.value || isCopying.value || openingNumber.value !== null || browse.isTransitioning.value,
)

const canGoNext = computed(() => step.value === 'source' && target.value !== null && !isBusy.value)

const sourceText = computed(() => {
  const current = diagnosedTarget.value
  if (current === null) {
    return ''
  }
  return current.kind === 'image'
    ? t('undelete.sourceImage', { path: current.path })
    : t('undelete.sourceDisk', { number: current.diskNumber })
})

const diagnosingText = computed(() =>
  diagnosedTarget.value?.kind === 'disk' ? t('repair.diagnose.runningDisk') : t('repair.diagnose.running'),
)

onMounted(() => {
  titleElement.value?.focus()
})

async function handleExit() {
  if (isCopying.value) {
    return
  }
  await closeBrowse()
  emit('exit')
}

async function goBack() {
  if (isBusy.value) {
    return
  }
  if (isBrowsing.value) {
    await closeBrowse()
    return
  }
  // 诊断这一步只在进行中或失败时停留，返回直接回到来源
  step.value = 'source'
  diagnoseFailure.value = null
}

async function handleSubmit() {
  if (canGoNext.value) {
    await runDiagnosis()
  }
}

/** 读各分区的主备引导扇区；读完进入结果，失败时留在诊断这一步，可以重试 */
async function runDiagnosis() {
  const current = step.value === 'source' ? target.value : diagnosedTarget.value
  if (current === null || isDiagnosing.value) {
    return
  }
  await closeBrowse()
  diagnosedTarget.value = current
  diagnosis.value = null
  diagnoseFailure.value = null
  browseError.value = null
  step.value = 'diagnose'
  isDiagnosing.value = true
  try {
    diagnosis.value = await diagnoseBoot(current)
    step.value = 'result'
  } catch (error) {
    diagnoseFailure.value = toHostError(error)
  } finally {
    isDiagnosing.value = false
  }
}

/** 按备份引导扇区打开浏览会话，进入这个分区；会话里这个分区仍打不开时留在结论列表，写明原因 */
async function browseWithBackup(partition: PartitionBoot) {
  const source = diagnosedTarget.value
  if (source === null || isBusy.value) {
    return
  }
  browseError.value = null
  openingNumber.value = partition.number
  try {
    await closeBrowse()
    await browse.open(source, 'backup')
    const opened = browse.opened.value?.partitions.find((candidate) => candidate.number === partition.number)
    if (browse.isOpen.value && opened?.browsable === true) {
      browsingNumber.value = partition.number
      return
    }
    const reason = browse.failure.value ? formatHostError(browse.failure.value) : t('repair.browse.notBrowsable')
    browseError.value = { number: partition.number, text: reason }
    await closeBrowse()
  } finally {
    openingNumber.value = null
  }
}

async function closeBrowse() {
  browsingNumber.value = null
  if (browse.isOpen.value) {
    await browse.close()
  }
}

function filesystemName(partition: PartitionBoot): string {
  const key = `partition.fs.${partition.filesystem}`
  return te(key) ? t(key) : partition.filesystem
}

/** 「分区 N · 卷标 · 文件系统 · 大小」，没有卷标时不留空位 */
function partitionTitle(partition: PartitionBoot): string {
  const number = t('repair.result.partitionTitle', {
    number: partition.number,
    logical: partition.logical ? ` ${t('undelete.partition.logical')}` : '',
  })
  const label = partition.label ?? partition.name
  return [number, label, filesystemName(partition), formatBytes(partition.length)]
    .filter((part) => part !== '')
    .join(' · ')
}

/** 分区表里的类型：常见的给本地化名称，MBR 另附类型字节 */
function partitionRole(partition: PartitionBoot): string {
  const key = `partition.role.${partition.role ?? ''}`
  if (partition.role === null || !te(key)) {
    return partition.typeId === '' ? '-' : partition.typeId
  }
  return partition.typeId.startsWith('0x')
    ? t('repair.result.roleWithType', { role: t(key), type: partition.typeId })
    : t(key)
}

function positionText(copy: BootCopyReport): string {
  const sectorSize = diagnosis.value?.sectorSize ?? 512
  return t('repair.result.position', {
    sector: Math.floor(copy.offset / sectorSize).toLocaleString(locale.value),
    offset: copy.offset.toLocaleString(locale.value),
  })
}

function volumeText(copy: BootCopyReport): string | null {
  if (copy.volumeBytes === null || copy.clusterSize === null) {
    return null
  }
  return t('repair.result.volume', {
    size: formatBytes(copy.volumeBytes),
    cluster: formatBytes(copy.clusterSize),
  })
}

function hex(value: number, digits: number): string {
  return `0x${value.toString(16).toUpperCase().padStart(digits, '0')}`
}

function plain(value: number | null): string {
  return value === null ? '' : value.toLocaleString(locale.value)
}

function issueText(issue: BootIssue): string {
  const key = `repair.issue.${issue.code}`
  if (!te(key)) {
    return issue.code
  }
  switch (issue.code) {
    case 'volumeLarger':
    case 'volumeSmaller':
      return t(key, { value: formatBytes(issue.value ?? 0), expected: formatBytes(issue.expected ?? 0) })
    case 'checksumMismatch':
      if (issue.value === null) {
        return t('repair.issue.checksumUneven', { expected: hex(issue.expected ?? 0, 8) })
      }
      return t(key, { value: hex(issue.value, 8), expected: hex(issue.expected ?? 0, 8) })
    case 'badMedia':
      return t(key, { value: hex(issue.value ?? 0, 2) })
    default:
      return t(key, { value: plain(issue.value), expected: plain(issue.expected) })
  }
}

/** 分区表与引导扇区不符：类型不符时写明两边各是什么 */
function tableIssueText(partition: PartitionBoot, issue: BootIssue): string {
  if (issue.code === 'typeMismatch') {
    return t('repair.issue.typeMismatch', {
      type: partitionRole(partition),
      filesystem: filesystemName(partition),
    })
  }
  return issueText(issue)
}

function fieldName(difference: BootDifference): string {
  const key = `repair.field.${difference.field}`
  return te(key) ? t(key) : difference.field
}

function fieldValue(difference: BootDifference, value: number): string {
  return HEX_FIELDS.includes(difference.field) ? hex(value, 2) : value.toLocaleString(locale.value)
}

/** 引导扇区正常、却打不开卷：问题在文件系统内部 */
function openFailure(partition: PartitionBoot): string | null {
  const copies = [partition.primary, partition.backup].filter(
    (copy): copy is BootCopyReport => copy !== null && copy.issues.length === 0 && copy.openError !== null,
  )
  const first = copies[0]
  return first === undefined ? null : t('repair.result.openFailed', { reason: first.openError })
}

/** 主引导扇区坏了、备份核对通过并且打得开：可以用备份浏览 */
function canBrowseBackup(partition: PartitionBoot): boolean {
  return partition.verdict === 'primaryDamaged' && partition.backup?.opened === true
}

function noteText(note: string): string {
  const key = `undelete.notes.${note}`
  return te(key) ? t(key) : note
}
</script>

<template>
  <section
    class="wizard"
    :class="{ 'wizard--fill': isBrowsing }"
    aria-labelledby="repair-wizard-title"
  >
    <header class="wizard__header">
      <button
        class="wizard__button wizard__button--secondary"
        type="button"
        :disabled="isCopying"
        @click="handleExit"
      >
        {{ t('nav.back') }}
      </button>
      <h2 id="repair-wizard-title" ref="titleElement" class="wizard__title" tabindex="-1">
        {{ t('features.repairPartition.name') }}
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
            {{ t(`repair.steps.${name}`) }}
          </li>
        </ol>
        <div class="wizard__nav">
          <button
            v-if="stepIndex > 0"
            class="wizard__button wizard__button--secondary"
            type="button"
            :disabled="isBusy"
            @click="goBack"
          >
            {{ isBrowsing ? t('repair.browse.backToResult') : t('carve.previous') }}
          </button>
          <button v-if="step === 'source'" class="wizard__button" type="submit" :disabled="!canGoNext">
            {{ t('repair.diagnose.start') }}
          </button>
          <button
            v-else-if="step === 'result' && !isBrowsing"
            class="wizard__button wizard__button--secondary"
            type="button"
            :disabled="isBusy"
            @click="runDiagnosis"
          >
            {{ t('repair.diagnose.again') }}
          </button>
        </div>
      </div>

      <div v-if="step === 'source'" class="wizard__panel">
        <p class="wizard__note">{{ t('repair.intro') }}</p>
        <SourcePicker
          v-model:kind="sourceKind"
          v-model:image-path="imagePath"
          v-model:disk="selectedDisk"
          :disk-hint="t('repair.diskHint')"
        />
      </div>

      <div v-else-if="step === 'diagnose'" class="wizard__panel" aria-live="polite">
        <p v-if="isDiagnosing" class="wizard__status">{{ diagnosingText }}</p>
        <template v-else-if="diagnoseFailure">
          <p class="wizard__error" role="alert">{{ formatHostError(diagnoseFailure) }}</p>
          <div class="wizard__retry">
            <button class="wizard__button" type="button" @click="runDiagnosis">
              {{ t('repair.diagnose.retry') }}
            </button>
          </div>
        </template>
      </div>

      <div
        v-else-if="isBrowsing && browsingPartition && browsingRef && browse.jobId.value && browse.target.value"
        class="wizard__panel wizard__fill"
      >
        <dl class="wizard__details">
          <dt>{{ t('undelete.browse.source') }}</dt>
          <dd class="wizard__path-text">{{ sourceText }}</dd>
          <dt>{{ t('undelete.browse.partition') }}</dt>
          <dd>
            {{
              t('undelete.browse.partitionValue', {
                number: browsingPartition.number,
                label: browsingPartition.label ?? '',
                filesystem: te(`partition.fs.${browsingPartition.filesystem}`)
                  ? t(`partition.fs.${browsingPartition.filesystem}`)
                  : browsingPartition.filesystem,
                size: formatBytes(browsingPartition.length),
              })
            }}
          </dd>
        </dl>
        <p class="wizard__hint wizard__hint--warning">{{ t('repair.browse.intro') }}</p>
        <p v-if="browse.target.value.kind === 'disk'" class="wizard__hint">{{ t('undelete.browse.liveHint') }}</p>
        <p v-if="browse.isUnresponsive.value" class="wizard__error" role="alert">
          {{ t('carve.run.unresponsive') }}
        </p>
        <p v-if="!browse.isOpen.value && browse.failure.value" class="wizard__error" role="alert">
          {{ formatHostError(browse.failure.value) }}
        </p>
        <div class="repair-wizard__files wizard__fill">
          <PartitionFiles
            v-model:busy="isCopying"
            :job-id="browse.jobId.value"
            :source="browse.target.value"
            :partition="browsingPartition"
            :partition-ref="browsingRef"
            :sector-size="browse.opened.value?.sectorSize ?? 512"
            :can-browse="browse.isOpen.value"
            boot="backup"
          />
        </div>
      </div>

      <div v-else-if="diagnosis" class="wizard__panel">
        <p class="wizard__note">
          {{
            t('undelete.partition.summary', {
              source: sourceText,
              scheme: t(`undelete.partition.scheme.${diagnosis.scheme}`),
              size: formatBytes(diagnosis.size),
            })
          }}
        </p>
        <p class="wizard__hint wizard__hint--warning">{{ t('repair.readOnly') }}</p>
        <ul v-if="diagnosis.notes.length" class="repair-wizard__list">
          <li v-for="note in diagnosis.notes" :key="note">{{ noteText(note) }}</li>
        </ul>
        <p v-if="diagnosis.partitions.length === 0" class="wizard__note">{{ t('repair.result.none') }}</p>

        <section
          v-for="partition in diagnosis.partitions"
          :key="partition.number"
          class="repair-wizard__partition"
          :aria-labelledby="`repair-partition-${partition.number}`"
        >
          <h3 :id="`repair-partition-${partition.number}`" class="repair-wizard__partition-title">
            {{ partitionTitle(partition) }}
          </h3>
          <p class="repair-wizard__verdict" :class="`repair-wizard__verdict--${partition.verdict}`">
            <strong>{{ t(`repair.verdict.${partition.verdict}.title`) }}</strong>
            {{ t(`repair.verdict.${partition.verdict}.detail`) }}
          </p>
          <dl class="wizard__details">
            <dt>{{ t('repair.result.type') }}</dt>
            <dd>
              {{ t('repair.result.typeValue', { type: partitionRole(partition), start: formatBytes(partition.offset) }) }}
            </dd>
            <template v-if="partition.verdict !== 'unsupported' && partition.verdict !== 'unrecognized'">
              <dt>{{ t('repair.result.primary') }}</dt>
              <dd>
                <span>{{ positionText(partition.primary) }}</span>
                <span v-if="volumeText(partition.primary)"> · {{ volumeText(partition.primary) }}</span>
                <p v-if="partition.primary.issues.length === 0" class="repair-wizard__ok">
                  {{ t('repair.result.copyOk') }}
                </p>
                <ul v-else class="repair-wizard__list">
                  <li v-for="(issue, index) in partition.primary.issues" :key="index">{{ issueText(issue) }}</li>
                </ul>
              </dd>
              <dt>{{ t('repair.result.backup') }}</dt>
              <dd>
                <template v-if="partition.backup">
                  <span>{{ positionText(partition.backup) }}</span>
                  <span v-if="volumeText(partition.backup)"> · {{ volumeText(partition.backup) }}</span>
                  <p v-if="partition.backup.issues.length === 0" class="repair-wizard__ok">
                    {{ t('repair.result.copyOk') }}
                  </p>
                  <ul v-else class="repair-wizard__list">
                    <li v-for="(issue, index) in partition.backup.issues" :key="index">{{ issueText(issue) }}</li>
                  </ul>
                </template>
                <template v-else>{{ t('repair.result.noBackup') }}</template>
              </dd>
            </template>
          </dl>

          <table v-if="partition.differences.length" class="wizard__table">
            <caption class="wizard__caption">{{ t('repair.result.differences') }}</caption>
            <thead>
              <tr>
                <th scope="col">{{ t('repair.result.field') }}</th>
                <th scope="col" class="wizard__number">{{ t('repair.result.primary') }}</th>
                <th scope="col" class="wizard__number">{{ t('repair.result.backup') }}</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="difference in partition.differences" :key="difference.field">
                <th scope="row">{{ fieldName(difference) }}</th>
                <td class="wizard__number">{{ fieldValue(difference, difference.primary) }}</td>
                <td class="wizard__number">{{ fieldValue(difference, difference.backup) }}</td>
              </tr>
            </tbody>
          </table>

          <template v-if="partition.tableIssues.length">
            <p class="repair-wizard__subtitle">{{ t('repair.result.tableIssues') }}</p>
            <ul class="repair-wizard__list">
              <li v-for="issue in partition.tableIssues" :key="issue.code">
                {{ tableIssueText(partition, issue) }}
              </li>
            </ul>
          </template>

          <p v-if="openFailure(partition)" class="wizard__hint">{{ openFailure(partition) }}</p>
          <p v-if="te(`repair.next.${partition.verdict}`)" class="wizard__hint">
            {{ t(`repair.next.${partition.verdict}`) }}
          </p>

          <div v-if="canBrowseBackup(partition)" class="repair-wizard__actions">
            <button
              class="wizard__button"
              type="button"
              :disabled="isBusy"
              @click="browseWithBackup(partition)"
            >
              {{ t('repair.browse.start') }}
            </button>
            <span v-if="openingNumber === partition.number" class="wizard__note" aria-live="polite">
              {{ t('repair.browse.opening') }}
            </span>
          </div>
          <p v-if="browseError?.number === partition.number" class="wizard__error" role="alert">
            {{ t('repair.browse.failed', { reason: browseError.text }) }}
          </p>
        </section>
      </div>
    </form>
  </section>
</template>

<style scoped>
.repair-wizard__partition {
  display: flex;
  flex-direction: column;
  gap: var(--space-sm);
  padding-top: var(--space-md);
  border-top: 1px solid var(--color-border);
}

.repair-wizard__partition-title {
  margin: 0;
  font-size: var(--font-size-body);
  font-weight: 600;
}

.repair-wizard__verdict {
  margin: 0;
}

.repair-wizard__verdict--backupDamaged strong,
.repair-wizard__verdict--primaryDamaged strong {
  color: var(--color-warning);
}

.repair-wizard__verdict--bothDamaged strong {
  color: var(--color-danger);
}

.repair-wizard__verdict--unsupported,
.repair-wizard__verdict--unrecognized {
  color: var(--color-text-muted);
}

.repair-wizard__list {
  margin: 0;
  padding-left: var(--space-lg);
  font-size: var(--font-size-small);
}

.repair-wizard__ok {
  margin: 0;
  font-size: var(--font-size-small);
}

.repair-wizard__subtitle {
  margin: 0;
  font-weight: 600;
}

.repair-wizard__actions {
  display: flex;
  align-items: center;
  gap: var(--space-sm);
}

/* PartitionFiles 的两栏与复制状态直接排进这个网格（同「找回删除的文件」） */
.repair-wizard__files {
  display: grid;
  grid-template-columns: minmax(0, 3fr) minmax(0, 2fr);
  grid-template-rows: auto 1fr auto;
  gap: var(--space-sm) var(--space-lg);
}
</style>
