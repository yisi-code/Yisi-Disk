<script setup lang="ts">
import { computed, onMounted, ref, useId, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import JobControls from './JobControls.vue'
import { useHostDialog } from '../composables/useHostDialog'
import { useHostErrorText } from '../composables/useHostErrorText'
import { useJob } from '../composables/useJob'
import { useWritePlanText } from '../composables/useWritePlanText'
import {
  rateFatCopies,
  startSaveLost,
  startScanFree,
  toHostError,
  type ClusterRun,
  type CopySummary,
  type FatRating,
  type FreeScanSummary,
  type FsCheckReport,
  type FsFix,
  type FsFixCount,
  type HostError,
  type JobTarget,
  type PartitionRef,
} from '../services/host'
import { formatBytes } from '../utils/format'

/**
 * 检查文件系统的结果页上，FAT / exFAT 能用写入计划修的几类问题（检查结果的 repairs）：按类勾选，生成计划后经确认页写入。
 * 丢失簇要先另存到别的盘（只读源）才能勾选释放；坏簇要先把空闲簇读一遍；两份 FAT 不一致时先把两种方向各预演一遍，
 * 选一份为准单独修
 */
const props = defineProps<{
  report: FsCheckReport
  target: JobTarget
  partition: PartitionRef
  /** raw 镜像文件或整块磁盘才能写；选的是一个盘符或 E01 镜像时只说明 */
  canWrite: boolean
  /** 别处正在忙（生成计划、检查）：不能再生成计划 */
  busy: boolean
  preparing: boolean
}>()
const emit = defineEmits<{ propose: [fixes: FsFix[], badClusters: ClusterRun[]] }>()
/** 另存丢失簇、读空闲簇进行中：页面不能返回、离开或重新检查 */
const saving = defineModel<boolean>('saving', { default: false })
/**
 * 勾选的几类、另存丢失簇与读空闲簇的结果、两份 FAT 的预演由使用方保存：确认页取消回来、面板重新挂载后还在；
 * 换了检查结果时由使用方清空
 */
const selected = defineModel<FsFix[]>('selected', { default: () => [] })
const saved = defineModel<CopySummary | null>('saved', { default: null })
const freeScan = defineModel<FreeScanSummary | null>('freeScan', { default: null })
const ratings = defineModel<FatRating[] | null>('ratings', { default: null })

/** 列出的次序；两份 FAT 的两个方向是一组单选 */
const ORDER: FsFix[] = [
  'rebuildBitmap',
  'lost',
  'bitmap',
  'tooLong',
  'chains',
  'crossLinks',
  'entries',
  'upcase',
  'fsInfo',
  'badClusters',
  'dirty',
]
const FAT_SOURCES: FsFix[] = ['fatFromFirst', 'fatFromSecond']
/** 每类修复对应检查里的哪几类问题（清除卸下标记前，看严重的问题是不是都勾上了） */
const ISSUE_KINDS: Record<FsFix, string[]> = {
  fsInfo: ['fsInfoInvalid'],
  bitmap: ['notInBitmap'],
  tooLong: ['chainTooLong'],
  lost: ['lostClusters', 'lostClustersMore'],
  dirty: ['volumeDirty'],
  fatFromFirst: ['fatCopiesDiffer'],
  fatFromSecond: ['fatCopiesDiffer'],
  chains: ['firstClusterInvalid', 'chainOutOfRange', 'chainLoop', 'chainFree', 'chainBadCluster', 'chainTooShort'],
  crossLinks: ['crossLink'],
  entries: [
    'entrySetChecksum',
    'entrySetCount',
    'entryNameLength',
    'orphanEntries',
    'lfnChecksum',
    'lfnOrder',
    'dotEntries',
    'sizeInvalid',
  ],
  badClusters: [],
  upcase: ['upcaseChecksum', 'upcaseMissing'],
  rebuildBitmap: ['bitmapInvalid'],
}
const FAT_FAMILY = ['fat12', 'fat16', 'fat32', 'exfat']

const { t, locale } = useI18n()
const { writeBlockedText } = useWritePlanText()
const { formatHostError } = useHostErrorText()
const { dialogError, pickFolder } = useHostDialog()
const domPrefix = useId()

const rating = ref(false)
const ratingError = ref<HostError | null>(null)
const save = useJob<CopySummary>({
  onCompleted(result) {
    saved.value = result
  },
})
const scan = useJob<FreeScanSummary>({
  onCompleted(result) {
    freeScan.value = result
  },
})

const isFat = computed(() => FAT_FAMILY.includes(props.report.filesystem))
const repairOf = (fix: FsFix): FsFixCount | undefined => props.report.repairs.find((repair) => repair.fix === fix)
/** 两份 FAT 不一致：只能先统一 */
const unifying = computed(() => FAT_SOURCES.some((fix) => repairOf(fix)))
/** 分配位图本身坏了：只能先重建（要单独做） */
const rebuilding = computed(() => repairOf('rebuildBitmap') !== undefined)
/** 这个分区上能修的几类；坏簇不在检查结果里，FAT 家族都能先读一遍空闲簇（位图坏了、两份 FAT 不一致时除外） */
const available = computed(() => {
  if (!isFat.value) {
    return []
  }
  return ORDER.filter((fix) =>
    fix === 'badClusters' ? !unifying.value && !rebuilding.value : repairOf(fix) !== undefined,
  )
})
/** 丢失簇存好了才能勾选释放；读出了读不出的空闲簇才能勾选标坏簇 */
const lostReady = computed(() => saved.value !== null && saved.value.failed === 0)
const badReady = computed(() => (freeScan.value?.unreadableClusters ?? 0) > 0)
/** 勾了清除标记，却还有没勾上修法的「会丢数据」「结构错误」：引擎预演后会拒绝，这里先说明 */
const dirtyBlocked = computed(() => {
  if (!selected.value.includes('dirty')) {
    return false
  }
  const covered = selected.value.flatMap((fix) => ISSUE_KINDS[fix])
  return props.report.issues.some((issue) => issue.severity !== 'minor' && !covered.includes(issue.kind))
})
const isWorking = computed(() => save.isBusy.value || scan.isBusy.value)
const canPropose = computed(
  () => props.canWrite && selected.value.length > 0 && !dirtyBlocked.value && !props.busy && !isWorking.value,
)
/** 修完问题最少的那个方向 */
const recommended = computed(() => (ratings.value === null ? null : bestOf(ratings.value)))

watch(isWorking, (busy) => {
  saving.value = busy
})
watch(lostReady, (ready) => {
  if (!ready) {
    selected.value = selected.value.filter((fix) => fix !== 'lost')
  }
})
watch(badReady, (ready) => {
  if (!ready) {
    selected.value = selected.value.filter((fix) => fix !== 'badClusters')
  }
})

onMounted(() => {
  void rate()
})

/** 两份 FAT 不一致时把两种方向各预演一遍（已有结果就不再算），默认选修完问题少的那个 */
async function rate() {
  if (!unifying.value || ratings.value !== null || rating.value) {
    return
  }
  rating.value = true
  ratingError.value = null
  try {
    const list = await rateFatCopies(props.target, props.partition)
    ratings.value = list
    // 模型的值要等使用方回传才更新，这里直接用这次的结果
    const best = bestOf(list)
    if (best !== null && !selected.value.some((fix) => FAT_SOURCES.includes(fix))) {
      pickSource(best)
    }
  } catch (error) {
    ratingError.value = toHostError(error)
  } finally {
    rating.value = false
  }
}

/** 修完剩下的问题最少的方向：先比会丢数据的，再比结构错误，最后比可以忽略的 */
function bestOf(list: FatRating[]): FsFix | null {
  if (list.length === 0) {
    return null
  }
  const left = (item: FatRating) => [item.totals.dataLoss, item.totals.structure, item.totals.minor]
  return list.reduce((best, item) => {
    const [a, b] = [left(item), left(best)]
    const index = a.findIndex((value, at) => value !== b[at])
    return index >= 0 && a[index] < b[index] ? item : best
  }).fix
}

function pickSource(fix: FsFix) {
  selected.value = [...selected.value.filter((item) => !FAT_SOURCES.includes(item)), fix]
}

/** 一类问题有多少：丢失簇、位图按簇数，交叉链接按文件与簇数，其余按处数 */
function countText(fix: FsFix): string {
  const repair = repairOf(fix)
  if (repair === undefined) {
    return ''
  }
  const items = repair.items.toLocaleString(locale.value)
  const clusters = repair.clusters.toLocaleString(locale.value)
  if (fix === 'lost' || fix === 'bitmap') {
    return t('fsCheck.fix.lostCount', { clusters }, repair.clusters)
  }
  if (fix === 'crossLinks') {
    return t('fsCheck.fix.crossCount', { items, clusters }, repair.items)
  }
  return t('fsCheck.fix.count', { count: items }, repair.items)
}

function labelText(fix: FsFix): string {
  const label = t(`fsCheck.fix.${fix}.label`)
  const count = countText(fix)
  return count === '' ? label : t('fsCheck.fix.labelWithCount', { label, count })
}

/** 一个方向修完还剩多少问题 */
function ratingText(fix: FsFix): string {
  const item = ratings.value?.find((entry) => entry.fix === fix)
  if (item === undefined) {
    return ''
  }
  const { dataLoss, structure, minor } = item.totals
  if (dataLoss + structure + minor === 0) {
    return t('fsCheck.fix.fatCopies.clean')
  }
  return t('fsCheck.fix.fatCopies.left', {
    dataLoss: dataLoss.toLocaleString(locale.value),
    structure: structure.toLocaleString(locale.value),
    minor: minor.toLocaleString(locale.value),
  })
}

async function saveLost() {
  if (isWorking.value) {
    return
  }
  const dir = await pickFolder(t('fsCheck.fix.saveTitle'))
  if (dir === null) {
    return
  }
  saved.value = null
  await save.start((jobId) => startSaveLost(jobId, props.target, props.partition, dir))
}

async function scanFree() {
  if (isWorking.value) {
    return
  }
  freeScan.value = null
  await scan.start((jobId) => startScanFree(jobId, props.target, props.partition))
}

function propose() {
  if (canPropose.value) {
    const fixes = [...FAT_SOURCES, ...ORDER].filter((fix) => selected.value.includes(fix))
    const bad = fixes.includes('badClusters') ? (freeScan.value?.unreadable ?? []) : []
    emit('propose', fixes, bad)
  }
}
</script>

<template>
  <section v-if="unifying || available.length > 0" class="fs-fix" :aria-labelledby="`${domPrefix}-title`">
    <h3 :id="`${domPrefix}-title`" class="wizard__section-title">{{ t('fsCheck.fix.title') }}</h3>
    <p class="wizard__hint">{{ t('fsCheck.fix.intro') }}</p>
    <fieldset class="wizard__choices" :disabled="!canWrite || busy">
      <legend class="wizard__visually-hidden">{{ t('fsCheck.fix.title') }}</legend>
      <div v-if="unifying" class="fs-fix__item">
        <span>
          {{ t('fsCheck.fix.fatCopies.label') }}
          <span class="wizard__note">
            {{ t('fsCheck.fix.fatCopies.hint', { sectors: (repairOf('fatFromFirst')?.items ?? 0).toLocaleString(locale) }) }}
          </span>
        </span>
        <label v-for="fix in FAT_SOURCES" :key="fix" class="wizard__choice fs-fix__sub">
          <input
            type="radio"
            :name="`${domPrefix}-fat`"
            :value="fix"
            :checked="selected.includes(fix)"
            :disabled="ratings === null"
            @change="pickSource(fix)"
          />
          <span>
            {{ t(`fsCheck.fix.fatCopies.${fix}`) }}
            <template v-if="fix === recommended">{{ t('fsCheck.fix.fatCopies.recommended') }}</template>
            <span class="wizard__note">{{ ratingText(fix) }}</span>
          </span>
        </label>
        <p v-if="rating" class="wizard__note fs-fix__sub" aria-live="polite">{{ t('fsCheck.fix.fatCopies.rating') }}</p>
        <p v-if="ratingError" class="wizard__error fs-fix__sub" role="alert">{{ formatHostError(ratingError) }}</p>
      </div>
      <div v-for="fix in available" :key="fix" class="fs-fix__item">
        <label class="wizard__choice">
          <input
            v-model="selected"
            type="checkbox"
            :value="fix"
            :disabled="(fix === 'lost' && !lostReady) || (fix === 'badClusters' && !badReady)"
          />
          <span>
            {{ labelText(fix) }}
            <span class="wizard__note">{{ t(`fsCheck.fix.${fix}.hint`) }}</span>
          </span>
        </label>
        <div v-if="fix === 'lost'" class="fs-fix__save">
          <template v-if="save.isBusy.value">
            <progress class="wizard__progress" :value="save.done.value" :max="save.total.value || 1"></progress>
            <span class="wizard__note" aria-live="polite">
              {{ t('fsCheck.fix.saving', { done: formatBytes(save.done.value), total: formatBytes(save.total.value) }) }}
            </span>
            <JobControls :status="save.status.value" @cancel="save.cancel" />
          </template>
          <template v-else>
            <button
              class="wizard__button wizard__button--secondary"
              type="button"
              :disabled="isWorking"
              @click="saveLost"
            >
              {{ t('fsCheck.fix.save') }}
            </button>
            <span v-if="saved" class="wizard__note" role="status">
              {{
                t('fsCheck.fix.saved', {
                  files: saved.files.toLocaleString(locale),
                  size: formatBytes(saved.bytes),
                  path: saved.outputDir,
                })
              }}
              <template v-if="saved.failed > 0">{{ t('fsCheck.fix.saveFailed', { count: saved.failed }) }}</template>
            </span>
            <span v-else class="wizard__hint">{{ t('fsCheck.fix.saveFirst') }}</span>
          </template>
        </div>
        <div v-if="fix === 'badClusters'" class="fs-fix__save">
          <template v-if="scan.isBusy.value">
            <progress class="wizard__progress" :value="scan.done.value" :max="scan.total.value || 1"></progress>
            <span class="wizard__note" aria-live="polite">
              {{ t('fsCheck.fix.badClusters.scanning', { done: formatBytes(scan.done.value), total: formatBytes(scan.total.value) }) }}
            </span>
            <JobControls :status="scan.status.value" @cancel="scan.cancel" />
          </template>
          <template v-else>
            <button
              class="wizard__button wizard__button--secondary"
              type="button"
              :disabled="isWorking"
              @click="scanFree"
            >
              {{ t('fsCheck.fix.badClusters.scan') }}
            </button>
            <span v-if="freeScan" class="wizard__note" role="status">
              {{
                freeScan.unreadableClusters > 0
                  ? t('fsCheck.fix.badClusters.scanned', {
                      size: formatBytes(freeScan.bytes),
                      count: freeScan.unreadableClusters.toLocaleString(locale),
                    })
                  : t('fsCheck.fix.badClusters.scannedClean', { size: formatBytes(freeScan.bytes) })
              }}
            </span>
            <span v-else class="wizard__hint">{{ t('fsCheck.fix.badClusters.scanFirst') }}</span>
          </template>
        </div>
      </div>
    </fieldset>
    <p v-if="save.failure.value" class="wizard__error" role="alert">{{ formatHostError(save.failure.value) }}</p>
    <p v-if="scan.failure.value" class="wizard__error" role="alert">{{ formatHostError(scan.failure.value) }}</p>
    <p v-if="dialogError" class="wizard__error" role="alert">{{ formatHostError(dialogError) }}</p>
    <div class="fs-fix__actions">
      <button class="wizard__button wizard__button--secondary" type="button" :disabled="!canPropose" @click="propose">
        {{ t('fsCheck.fix.propose') }}
      </button>
      <span v-if="preparing" class="wizard__note" aria-live="polite">{{ t('fsCheck.fix.preparing') }}</span>
      <span v-else-if="!canWrite" class="wizard__hint">{{ writeBlockedText(target, 'fsCheck.repair') }}</span>
      <span v-else-if="dirtyBlocked" class="wizard__hint wizard__hint--warning">{{ t('fsCheck.fix.dirtyBlocked') }}</span>
      <span v-else-if="unifying" class="wizard__hint">{{ t('fsCheck.fix.fatCopies.alone') }}</span>
      <span v-else-if="rebuilding" class="wizard__hint">{{ t('fsCheck.fix.rebuildBitmap.alone') }}</span>
    </div>
  </section>
</template>

<style scoped>
.fs-fix {
  display: flex;
  flex-direction: column;
  gap: var(--space-sm);
  width: 100%;
}

.fs-fix__item {
  display: flex;
  flex-direction: column;
  gap: var(--space-xs);
}

.fs-fix__save,
.fs-fix__actions {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-sm);
}

.fs-fix__save,
.fs-fix__sub {
  padding-left: var(--space-lg);
}

.fs-fix__sub {
  margin: 0;
}
</style>
