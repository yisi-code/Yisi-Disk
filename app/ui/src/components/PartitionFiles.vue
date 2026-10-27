<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import FsBrowser, { type TreeItem } from './FsBrowser.vue'
import FsEntryDetails from './FsEntryDetails.vue'
import JobControls from './JobControls.vue'
import FailureList from './FailureList.vue'
import ReadHealthHint from './ReadHealthHint.vue'
import { useCopy } from '../composables/useCopy'
import { useHostErrorText } from '../composables/useHostErrorText'
import { useHostDialog } from '../composables/useHostDialog'
import {
  openCopiedFile,
  openOrReveal,
  revealCopiedFile,
  startCopy as startCopyJob,
  toHostError,
  type BootCopy,
  type CopyItem,
  type HostError,
  type JobTarget,
  type PartitionInfo,
  type PartitionRef,
} from '../services/host'
import { formatBytes, formatSeconds } from '../utils/format'

/**
 * 一个分区里的文件：左边目录树（勾选、只看已删除的文件、另存为），右边详情与预览，下面是复制的进度与结果。
 * 「找回删除的文件」「找回丢失的分区」「修复打不开的分区」共用。根节点 display: contents，两栏与复制状态直接排进父级的网格：
 * 父级给出 grid-template-rows: auto 1fr auto（两栏各占三行、用 subgrid 对齐），最后两列是这两栏
 */

/** 复制失败的文件最多列出这么多个 */
const MAX_LISTED_FAILURES = 20
/** 为打开而复制时，宿主拒绝写程序目录下的 open-cache（例如程序就在源盘上），改请用户选保存位置 */
const OPEN_COPY_REFUSED = ['outputOnSource', 'outputUnavailable']
const FINISHED_STATUSES: string[] = ['completed', 'failed', 'cancelled']

const props = defineProps<{
  /** 浏览任务 */
  jobId: string
  /** 浏览会话的源：复制任务要用同一个源 */
  source: JobTarget
  partition: PartitionInfo
  /** 复制时交给引擎核对的分区位置 */
  partitionRef: PartitionRef
  /** 源的逻辑扇区大小 */
  sectorSize: number
  /** 浏览会话还开着：关了就不能打开、复制 */
  canBrowse: boolean
  /** 浏览会话按哪一份引导扇区打开，复制时用同一份；不给时为主引导扇区 */
  boot?: BootCopy
}>()
/** 复制进行中：父组件据此禁用返回、离开 */
const isBusy = defineModel<boolean>('busy', { default: false })

const { t } = useI18n()
const { formatHostError } = useHostErrorText()
const copy = useCopy()

const deletedOnly = ref(false)
const selectedItem = ref<TreeItem | null>(null)
const checkedItems = ref(new Map<string, TreeItem>())
const { dialogError, pickFolder } = useHostDialog()
const fileActionError = ref<HostError | null>(null)
const fileActionNote = ref<string | null>(null)
/** 当前的复制任务是另存为，还是为「打开」而复制单个文件 */
const copyPurpose = ref<'saveAs' | 'open'>('saveAs')
/** 为打开而复制的文件，复制好就打开它 */
const openingItem = ref<TreeItem | null>(null)
/** 当前的复制任务写的是程序目录下的 open-cache */
const isOpenCacheCopy = ref(false)

/** 勾选的项里，上级目录也被勾选的不再单列：目录本来就连同内容一起复制 */
const copyItems = computed<CopyItem[]>(() => {
  const ids = [...checkedItems.value.keys()]
  return [...checkedItems.value.values()]
    .filter((item) => !ids.some((other) => other !== item.id && item.id.startsWith(`${other}/`)))
    .map((item) => ({ file: item.entry.file, path: item.path }))
})

/** 复制出来全是 0 的文件 */
const zeroFiles = computed(() => copy.succeeded.value.filter((file) => file.allZero))

const canCopy = computed(
  () => copyItems.value.length > 0 && !copy.isBusy.value && props.canBrowse,
)

const copyStatusText = computed(() => {
  if (copyPurpose.value === 'open') {
    return t('common.preparing')
  }
  switch (copy.status.value) {
    case 'starting':
      return t('partitionFiles.copy.status.starting')
    case 'running':
    case 'cancelling':
      if (copy.stage.value === 'scanMft') {
        return t('partitionFiles.copy.status.scanning', { percent: copy.percent.value })
      }
      return t('partitionFiles.copy.status.running', {
        done: formatBytes(copy.done.value),
        total: formatBytes(copy.total.value),
        percent: copy.percent.value,
        count: copy.succeeded.value.length,
      }, copy.succeeded.value.length)
    case 'completed':
      return t('partitionFiles.copy.status.completed')
    case 'failed':
      return t('partitionFiles.copy.status.failed')
    case 'cancelled':
      return t('partitionFiles.copy.status.cancelled')
  }
  return ''
})

/** 另存为的进度与结果一直显示；为打开而复制只在进行中显示一行状态 */
const isCopyStatusShown = computed(
  () => copy.status.value !== 'idle' && (copyPurpose.value === 'saveAs' || copy.isBusy.value),
)

watch(
  [() => props.jobId, () => props.partition.number],
  () => {
    selectedItem.value = null
    checkedItems.value = new Map()
  },
)

watch(selectedItem, () => {
  fileActionError.value = null
  fileActionNote.value = null
})

watch(
  copy.isBusy,
  (busy) => {
    isBusy.value = busy
  },
  { immediate: true },
)

watch(copy.status, (status) => {
  const item = openingItem.value
  if (item !== null && FINISHED_STATUSES.includes(status)) {
    void finishOpen(item)
  }
})

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
    const dir = await pickFolder(t('partitionFiles.copy.fallbackTitle'))
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
    const outcome = await openOrReveal(
      () => openCopiedFile(jobId, first.path),
      () => revealCopiedFile(jobId, first.path),
    )
    if (outcome === 'revealed') {
      fileActionNote.value = t('partitionFiles.details.executableRevealed')
    }
  } catch (error) {
    fileActionError.value = toHostError(error)
  }
}

/** 另存为：每次都选保存位置，选好就开始复制 */
async function handleSaveAs() {
  if (!canCopy.value) {
    return
  }
  const dir = await pickFolder(t('partitionFiles.copy.pickTitle'))
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
  copyPurpose.value = purpose
  isOpenCacheCopy.value = dir === null
  const source = props.source
  const partitionRef = props.partitionRef
  const boot = props.boot ?? 'primary'
  await copy.start((jobId) => startCopyJob(jobId, source, partitionRef, items, dir, boot))
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
</script>

<template>
  <div class="partition-files">
    <FsBrowser
      v-model:selected="selectedItem"
      v-model:checked="checkedItems"
      class="partition-files__pane"
      :job-id="jobId"
      :partition="partition.number"
      :deleted-only="deletedOnly"
      @open="openItem"
    >
      <template #tools>
        <label class="partition-files__filter">
          <input v-model="deletedOnly" type="checkbox" />
          {{ t('partitionFiles.browse.deletedOnly') }}
        </label>
      </template>
      <template #actions>
        <div class="partition-files__save-as">
          <button class="wizard__button" type="button" :disabled="!canCopy" @click="handleSaveAs">
            {{ t('partitionFiles.copy.saveAs', { count: copyItems.length }) }}
          </button>
          <JobControls v-if="copy.isBusy.value" :status="copy.status.value" @cancel="copy.cancel" />
        </div>
      </template>
    </FsBrowser>
    <FsEntryDetails
      class="partition-files__pane"
      :job-id="jobId"
      :partition="partition.number"
      :item="selectedItem"
      :sector-size="sectorSize"
      :times-utc="partition.timesUtc"
      :can-open="!copy.isBusy.value && canBrowse"
      :action-error="fileActionError"
      :action-note="fileActionNote"
      @open="openItem"
    >
      <template #footer>
        <div class="partition-files__pane-notes">
          <p class="wizard__hint">{{ t('partitionFiles.browse.stateHint') }}</p>
          <p v-if="partition.timesUtc" class="wizard__hint">
            {{ t('partitionFiles.browse.timesUtc') }}
          </p>
        </div>
      </template>
    </FsEntryDetails>

    <div v-if="isCopyStatusShown" class="wizard__job partition-files__wide" aria-live="polite">
      <progress
        v-if="copyPurpose === 'saveAs'"
        class="wizard__progress"
        :value="copy.done.value"
        :max="copy.total.value || 1"
      ></progress>
      <p class="wizard__status">{{ copyStatusText }}</p>
      <ReadHealthHint
        :health="copy.readHealth.value"
        :source-is-disk="source.kind === 'disk'"
        context="scan"
      />
      <p v-if="copy.failure.value" class="wizard__error" role="alert">
        {{ formatHostError(copy.failure.value) }}
      </p>
      <template v-if="zeroFiles.length > 0">
        <p class="wizard__hint wizard__hint--warning">
          {{ t('partitionFiles.copy.allZero', { count: zeroFiles.length }, zeroFiles.length) }}
        </p>
        <ul class="wizard__failures">
          <li v-for="file in zeroFiles.slice(0, MAX_LISTED_FAILURES)" :key="file.path">
            {{ file.path }}
          </li>
        </ul>
      </template>
      <FailureList :failures="copy.failures.value" count-key="partitionFiles.copy.failedCount" />
      <dl v-if="copy.summary.value" class="wizard__details">
        <dt>{{ t('partitionFiles.copy.report.files') }}</dt>
        <dd>
          {{
            t('common.filesValue', {
              count: copy.summary.value.files,
              bytes: formatBytes(copy.summary.value.bytes),
            }, copy.summary.value.files)
          }}
        </dd>
        <dt>{{ t('common.elapsed') }}</dt>
        <dd>
          {{
            t('common.seconds', {
              seconds: formatSeconds(copy.summary.value.elapsedMs),
            })
          }}
        </dd>
        <dt>{{ t('common.savedIn') }}</dt>
        <dd class="wizard__path-text">{{ copy.summary.value.outputDir }}</dd>
      </dl>
      <div v-if="copy.summary.value && copy.succeeded.value.length > 0">
        <button
          class="wizard__button wizard__button--secondary"
          type="button"
          @click="revealOutput"
        >
          {{ t('common.reveal') }}
        </button>
      </div>
    </div>

    <p v-if="dialogError" class="wizard__error partition-files__wide" role="alert">
      {{ formatHostError(dialogError) }}
    </p>
  </div>
</template>

<style scoped>
.partition-files {
  display: contents;
}

/* 两栏各分标题、框体、底部三行，与父级网格的三行对齐 */
.partition-files__pane {
  display: grid;
  grid-row: span 3;
  grid-template-rows: subgrid;
}

/* 复制状态与对话框错误排在两栏下方，宽度与两栏合起来一样 */
.partition-files__wide {
  grid-column: -3 / -1;
}

.partition-files__filter {
  display: flex;
  align-items: center;
  gap: var(--space-xs);
  cursor: pointer;
}

.partition-files__save-as {
  display: flex;
  flex: none;
  gap: var(--space-sm);
}

.partition-files__pane-notes {
  display: flex;
  flex-direction: column;
  gap: var(--space-xs);
}
</style>
