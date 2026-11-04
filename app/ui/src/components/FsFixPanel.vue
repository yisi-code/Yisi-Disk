<script setup lang="ts">
import { computed, ref, useId, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import JobControls from './JobControls.vue'
import { useHostDialog } from '../composables/useHostDialog'
import { useHostErrorText } from '../composables/useHostErrorText'
import { useJob } from '../composables/useJob'
import {
  startSaveLost,
  type CopySummary,
  type FsCheckReport,
  type FsFix,
  type JobTarget,
  type PartitionRef,
} from '../services/host'
import { formatBytes } from '../utils/format'

/**
 * 检查文件系统的结果页上，FAT / exFAT 能用写入计划修的几类问题：按类勾选，生成计划后经确认页写入。
 * 丢失簇要先另存到别的盘（只读源）才能勾选释放
 */
const props = defineProps<{
  report: FsCheckReport
  target: JobTarget
  partition: PartitionRef
  /** 镜像文件或整块磁盘才能写；选的是一个盘符时只说明 */
  canWrite: boolean
  /** 别处正在忙（生成计划、检查）：不能再生成计划 */
  busy: boolean
  preparing: boolean
}>()
const emit = defineEmits<{ propose: [fixes: FsFix[]] }>()
/** 另存丢失簇进行中：页面不能返回、离开或重新检查 */
const saving = defineModel<boolean>('saving', { default: false })

const ORDER: FsFix[] = ['lost', 'bitmap', 'tooLong', 'fsInfo', 'dirty']
/** 每类修复对应检查里的哪几类问题 */
const ISSUE_KINDS: Record<FsFix, string[]> = {
  fsInfo: ['fsInfoInvalid'],
  bitmap: ['notInBitmap'],
  tooLong: ['chainTooLong'],
  lost: ['lostClusters', 'lostClustersMore'],
  dirty: ['volumeDirty'],
}
const FAT_FAMILY = ['fat12', 'fat16', 'fat32', 'exfat']

const { t, locale } = useI18n()
const { formatHostError } = useHostErrorText()
const { dialogError, pickFolder } = useHostDialog()
const domPrefix = useId()

const selected = ref<FsFix[]>([])
const saved = ref<CopySummary | null>(null)
const save = useJob<CopySummary>({
  onCompleted(result) {
    saved.value = result
  },
})

const issuesOf = (fix: FsFix) => props.report.issues.filter((issue) => ISSUE_KINDS[fix].includes(issue.kind))
/** 这个分区上能修的几类（FAT12 没有卸下标志） */
const available = computed(() => {
  const filesystem = props.report.filesystem
  if (!FAT_FAMILY.includes(filesystem)) {
    return []
  }
  return ORDER.filter((fix) => issuesOf(fix).length > 0 && !(fix === 'dirty' && filesystem === 'fat12'))
})
/** 丢失簇存好了才能勾选释放 */
const lostReady = computed(() => saved.value !== null && saved.value.failed === 0)
/** 勾了清除标记，却还有没勾上修法的「会丢数据」「结构错误」：引擎预演后会拒绝，这里先说明 */
const dirtyBlocked = computed(() => {
  if (!selected.value.includes('dirty')) {
    return false
  }
  const covered = selected.value.flatMap((fix) => ISSUE_KINDS[fix])
  return props.report.issues.some((issue) => issue.severity !== 'minor' && !covered.includes(issue.kind))
})
const canPropose = computed(
  () => props.canWrite && selected.value.length > 0 && !dirtyBlocked.value && !props.busy && !save.isBusy.value,
)

// 换了检查结果（重新检查、写完之后）：勾选与另存的结果都作废
watch(
  () => props.report,
  () => {
    selected.value = []
    saved.value = null
  },
)
watch(save.isBusy, (busy) => {
  saving.value = busy
})
watch(lostReady, (ready) => {
  if (!ready) {
    selected.value = selected.value.filter((fix) => fix !== 'lost')
  }
})

/** 一类问题有多少：丢失簇按簇数，其余按问题条数 */
function countText(fix: FsFix): string {
  const issues = issuesOf(fix)
  if (fix === 'lost') {
    const clusters = issues.reduce((sum, issue) => sum + (issue.count ?? 0), 0)
    return t('fsCheck.fix.lostCount', { clusters: clusters.toLocaleString(locale.value) }, clusters)
  }
  return t('fsCheck.fix.count', { count: issues.length.toLocaleString(locale.value) }, issues.length)
}

async function saveLost() {
  if (save.isBusy.value) {
    return
  }
  const dir = await pickFolder(t('fsCheck.fix.saveTitle'))
  if (dir === null) {
    return
  }
  saved.value = null
  await save.start((jobId) => startSaveLost(jobId, props.target, props.partition, dir))
}

function propose() {
  if (canPropose.value) {
    emit(
      'propose',
      ORDER.filter((fix) => selected.value.includes(fix)),
    )
  }
}
</script>

<template>
  <section v-if="available.length > 0" class="fs-fix" :aria-labelledby="`${domPrefix}-title`">
    <h3 :id="`${domPrefix}-title`" class="wizard__section-title">{{ t('fsCheck.fix.title') }}</h3>
    <p class="wizard__hint">{{ t('fsCheck.fix.intro') }}</p>
    <fieldset class="wizard__choices" :disabled="!canWrite || busy">
      <legend class="wizard__visually-hidden">{{ t('fsCheck.fix.title') }}</legend>
      <div v-for="fix in available" :key="fix" class="fs-fix__item">
        <label class="wizard__choice">
          <input
            v-model="selected"
            type="checkbox"
            :value="fix"
            :disabled="fix === 'lost' && !lostReady"
          />
          <span>
            {{ t(`fsCheck.fix.${fix}.label`) }}（{{ countText(fix) }}）
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
            <button class="wizard__button wizard__button--secondary" type="button" @click="saveLost">
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
      </div>
    </fieldset>
    <p v-if="save.failure.value" class="wizard__error" role="alert">{{ formatHostError(save.failure.value) }}</p>
    <p v-if="dialogError" class="wizard__error" role="alert">{{ formatHostError(dialogError) }}</p>
    <div class="fs-fix__actions">
      <button class="wizard__button wizard__button--secondary" type="button" :disabled="!canPropose" @click="propose">
        {{ t('fsCheck.fix.propose') }}
      </button>
      <span v-if="preparing" class="wizard__note" aria-live="polite">{{ t('fsCheck.fix.preparing') }}</span>
      <span v-else-if="!canWrite" class="wizard__hint">{{ t('fsCheck.repair.volumeUnsupported') }}</span>
      <span v-else-if="dirtyBlocked" class="wizard__hint wizard__hint--warning">{{ t('fsCheck.fix.dirtyBlocked') }}</span>
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

.fs-fix__save {
  padding-left: var(--space-lg);
}
</style>
