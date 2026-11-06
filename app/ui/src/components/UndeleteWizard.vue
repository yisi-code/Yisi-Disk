<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import DeletedScanProgress from './DeletedScanProgress.vue'
import PartitionFiles from './PartitionFiles.vue'
import PartitionPicker from './PartitionPicker.vue'
import SourcePicker from './SourcePicker.vue'
import WizardSteps from './WizardSteps.vue'
import { useBrowse } from '../composables/useBrowse'
import { useHostErrorText } from '../composables/useHostErrorText'
import { usePartitionText } from '../composables/usePartitionText'
import { useSourceSelection } from '../composables/useSourceSelection'
import { isDeviceKind, type PartitionRef } from '../services/host'
import { formatBytes, formatCount } from '../utils/format'

const STEPS = ['source', 'partition', 'browse'] as const
type WizardStep = (typeof STEPS)[number]

const emit = defineEmits<{ exit: [] }>()

const { t, locale } = useI18n()
const { formatHostError } = useHostErrorText()
const { tableNote, sourceName, unbrowsableReason, partitionLine } = usePartitionText()
const browse = useBrowse()

const titleElement = ref<HTMLHeadingElement | null>(null)
const step = ref<WizardStep>('source')
const { sourceKind, imagePath, device, folder, folderLocation, target } = useSourceSelection()
const selectedPartition = ref<number | null>(null)
/** 选文件夹找回时，打开的那一刻文件夹在卷里的位置：浏览时直接定位到它 */
const openedFolder = ref<string[] | null>(null)
/** 复制进行中（由 PartitionFiles 给出）：不能返回或离开 */
const isCopying = ref(false)

const stepIndex = computed(() => STEPS.indexOf(step.value))


const partitions = computed(() => browse.opened.value?.partitions ?? [])
const currentPartition = computed(
  () => partitions.value.find((partition) => partition.number === selectedPartition.value) ?? null,
)
const isSourceDisk = computed(() => isDeviceKind(browse.target.value?.kind))

const canGoNext = computed(() => {
  switch (step.value) {
    case 'source':
      return target.value !== null && !browse.isTransitioning.value
    case 'partition':
      return currentPartition.value?.browsable === true && !browse.isScanning.value
    case 'browse':
      return false
  }
  return false
})

/** 当前分区扫描已删除项的结果（NTFS 进来前就扫过；FAT / exFAT 点了「查找」才有） */
const currentScan = computed(() =>
  currentPartition.value === null
    ? null
    : (browse.scanResults.value.get(currentPartition.value.number) ?? null),
)
/** FAT / exFAT 读空闲簇扫完后加一，文件列表从根目录重新列出（根目录多出「找不到原目录的文件」） */
const filesReloadKey = ref(0)

/** FAT / exFAT 读空闲簇的结果：读了多少、找到几个文件夹、读不出几个簇 */
/** 分区可以读空闲空间找原文件夹已不在的文件，而且还没读过、没在读 */
const canScanFree = computed(
  () =>
    currentPartition.value?.optionalScan === true &&
    !browse.isScanning.value &&
    currentScan.value === null,
)

const freeScanText = computed(() => {
  const scan = currentScan.value
  if (scan === null) {
    return ''
  }
  const bytes = formatBytes(scan.freeBytes)
  const found =
    scan.orphans > 0
      ? t(
          'partitionFiles.scan.free.summary',
          {
            bytes,
            folders: formatCount(scan.orphans, locale.value),
            entries: formatCount(scan.deletedNames, locale.value),
          },
          scan.orphans,
        )
      : t('partitionFiles.scan.free.none', { bytes })
  if (scan.unreadable === 0) {
    return found
  }
  const unreadable = t(
    'partitionFiles.scan.free.unreadable',
    { count: formatCount(scan.unreadable, locale.value) },
    scan.unreadable,
  )
  // 与 NTFS 的扫描结果一样，几句之间空一格
  return `${found} ${unreadable}`
})
const needsScanHint = computed(() => partitions.value.some((partition) => partition.needsScan))

const partitionRef = computed<PartitionRef | null>(() =>
  currentPartition.value === null
    ? null
    : {
        number: currentPartition.value.number,
        offset: currentPartition.value.offset,
        length: currentPartition.value.length,
      },
)

const sourceText = computed(() => sourceName(browse.target.value))

const openingText = computed(() =>
  isDeviceKind(target.value?.kind) ? t('partition.openingDisk') : t('partition.opening'),
)

onMounted(() => {
  titleElement.value?.focus()
})

async function handleExit() {
  if (isCopying.value) {
    return
  }
  if (browse.isOpen.value) {
    await browse.close()
  }
  emit('exit')
}

async function goBack() {
  const previous = STEPS[stepIndex.value - 1]
  if (previous === undefined || isCopying.value || browse.isScanning.value) {
    return
  }
  // 回到来源这一步就关闭浏览：换了源要重新读分区表
  if (previous === 'source' && browse.isOpen.value) {
    await browse.close()
  }
  step.value = previous
}

async function handleSubmit() {
  if (step.value === 'source') {
    await openSource()
  } else if (step.value === 'partition' && canGoNext.value) {
    await enterPartition()
  }
}

/** NTFS 分区先扫一遍 MFT 找出已删除的项（扫过的直接进入），被取消或失败时留在分区这一步 */
async function enterPartition() {
  const partition = currentPartition.value
  if (partition === null) {
    return
  }
  if (partition.needsScan && !browse.scanResults.value.has(partition.number)) {
    const result = await browse.scan(partition)
    if (result === null || step.value !== 'partition') {
      return
    }
  }
  step.value = 'browse'
}

/** FAT / exFAT：读一遍空闲簇，找出原来的目录已找不到的文件夹；扫完重新列出文件 */
async function scanFreeClusters() {
  const partition = currentPartition.value
  if (partition === null || isCopying.value) {
    return
  }
  const result = await browse.scan(partition)
  if (result !== null) {
    filesReloadKey.value += 1
  }
}

async function openSource() {
  const current = target.value
  if (current === null || !canGoNext.value) {
    return
  }
  const folderPath = sourceKind.value === 'folder' ? (folderLocation.value?.path ?? null) : null
  await browse.open(current)
  if (!browse.isOpen.value) {
    return
  }
  openedFolder.value = folderPath
  selectedPartition.value =
    partitions.value.find((partition) => partition.browsable)?.number ?? null
  step.value = 'partition'
  // 选的是文件夹：它所在的卷就是唯一的分区，直接扫描、进去定位到文件夹
  if (folderPath !== null && canGoNext.value) {
    await enterPartition()
  }
}
</script>

<template>
  <section
    class="wizard"
    :class="{ 'wizard--fill': step === 'browse' }"
    aria-labelledby="undelete-wizard-title"
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
      <h2 id="undelete-wizard-title" ref="titleElement" class="wizard__title" tabindex="-1">
        {{ t('features.undelete.name') }}
      </h2>
    </header>

    <form class="wizard__form" @submit.prevent="handleSubmit">
      <div class="wizard__stepbar">
        <WizardSteps :steps="STEPS" :current="step" label-prefix="undelete.steps" />
        <div class="wizard__nav">
          <button
            v-if="stepIndex > 0"
            class="wizard__button wizard__button--secondary"
            type="button"
            :disabled="isCopying || browse.isScanning.value"
            @click="goBack"
          >
            {{ t('common.previous') }}
          </button>
          <button v-if="step !== 'browse'" class="wizard__button" type="submit" :disabled="!canGoNext">
            {{ t('common.next') }}
          </button>
        </div>
      </div>

      <div v-if="step === 'source'" class="wizard__panel">
        <SourcePicker
          v-model:kind="sourceKind"
          v-model:image-path="imagePath"
          v-model:device="device"
          v-model:folder="folder"
          :disk-hint="t('undelete.diskHint')"
          :folder-hint="t('undelete.folderHint')"
        />
        <p
          v-if="sourceKind === 'folder' && folder.trim() !== '' && folderLocation === null"
          class="wizard__error"
          role="alert"
        >
          {{ t('undelete.folderInvalid') }}
        </p>
        <p v-if="browse.status.value === 'starting'" class="wizard__note" aria-live="polite">
          {{ openingText }}
        </p>
        <p v-if="browse.failure.value" class="wizard__error" role="alert">
          {{ formatHostError(browse.failure.value) }}
        </p>
      </div>

      <div v-else-if="step === 'partition'" class="wizard__panel">
        <p class="wizard__note">
          {{
            t('partition.picker.summary', {
              source: sourceText,
              scheme: t(`partition.picker.scheme.${browse.opened.value?.scheme ?? 'none'}`),
              size: formatBytes(browse.opened.value?.size ?? 0),
            })
          }}
        </p>
        <ul v-if="browse.opened.value?.notes.length" class="wizard__list">
          <li v-for="note in browse.opened.value.notes" :key="note">{{ tableNote(note) }}</li>
        </ul>
        <PartitionPicker
          v-model="selectedPartition"
          :partitions="partitions"
          :caption="t('partition.picker.caption')"
          :is-selectable="(partition) => partition.browsable"
          :note="(partition) => (partition.browsable ? '' : unbrowsableReason(partition.filesystem))"
          :disabled="browse.isScanning.value"
        />
        <p v-if="!partitions.some((partition) => partition.browsable)" class="wizard__note">
          {{ t('partition.picker.none') }}
        </p>
        <p v-if="needsScanHint" class="wizard__hint">{{ t('partition.picker.scanHint') }}</p>
        <DeletedScanProgress v-if="browse.isScanning.value" :browse="browse" aria-live="polite" />
        <p v-if="browse.scanFailure.value" class="wizard__error" role="alert">
          {{ t('partitionFiles.scan.failed', { reason: formatHostError(browse.scanFailure.value) }) }}
        </p>
      </div>

      <div v-else-if="currentPartition && browse.jobId.value" class="wizard__panel wizard__fill">
        <!-- 来源、分区与「查找」按钮排在一行；按钮的说明悬停可看，读屏软件读下面那段隐藏的全文 -->
        <div class="undelete__top">
          <dl class="wizard__details wizard__details--inline">
            <div>
              <dt>{{ t('common.source') }}</dt>
              <dd class="wizard__path-text">{{ sourceText }}</dd>
            </div>
            <div>
              <dt>{{ t('partitionFiles.browse.partition') }}</dt>
              <dd>{{ partitionLine(currentPartition) }}</dd>
            </div>
          </dl>
          <button
            v-if="canScanFree"
            class="wizard__button wizard__button--secondary"
            type="button"
            :title="t('partitionFiles.scan.free.hint')"
            aria-describedby="undelete-free-hint"
            :disabled="isCopying || !browse.isOpen.value"
            @click="scanFreeClusters"
          >
            {{ t('partitionFiles.scan.free.action') }}
          </button>
        </div>
        <template v-if="currentPartition.optionalScan">
          <DeletedScanProgress v-if="browse.isScanning.value" :browse="browse" aria-live="polite" />
          <p v-else-if="currentScan" class="wizard__note">{{ freeScanText }}</p>
          <p
            v-if="browse.scanFailure.value && browse.scanKind.value === 'free'"
            class="wizard__error"
            role="alert"
          >
            {{ t('partitionFiles.scan.free.failed', { reason: formatHostError(browse.scanFailure.value) }) }}
          </p>
        </template>
        <p v-else-if="currentScan" class="wizard__note">
          {{
            t('partitionFiles.scan.summary', {
              records: currentScan.records.toLocaleString(locale),
              deleted: currentScan.deleted.toLocaleString(locale),
            })
          }}
          <template v-if="currentScan.logNames > 0">
            {{ t('partitionFiles.scan.fromLog', { count: currentScan.logNames.toLocaleString(locale) }, currentScan.logNames) }}
          </template>
          <template v-if="currentScan.orphans > 0">
            {{ t('partitionFiles.scan.orphans', { count: currentScan.orphans.toLocaleString(locale) }, currentScan.orphans) }}
          </template>
          <template v-if="currentScan.unreadable > 0">
            {{ t('partitionFiles.scan.unreadable', { count: currentScan.unreadable.toLocaleString(locale) }, currentScan.unreadable) }}
          </template>
        </p>
        <p v-if="browse.isUnresponsive.value" class="wizard__error" role="alert">
          {{ t('common.unresponsive') }}
        </p>
        <p v-if="!browse.isOpen.value && browse.failure.value" class="wizard__error" role="alert">
          {{ formatHostError(browse.failure.value) }}
        </p>
        <div class="wizard__files wizard__fill">
          <PartitionFiles
            v-if="browse.target.value && partitionRef"
            v-model:busy="isCopying"
            :job-id="browse.jobId.value"
            :source="browse.target.value"
            :partition="currentPartition"
            :partition-ref="partitionRef"
            :sector-size="browse.opened.value?.sectorSize ?? 512"
            :can-browse="browse.isOpen.value"
            :reveal-path="openedFolder ?? undefined"
            :reload-key="filesReloadKey"
          />
        </div>
        <!-- 固定的说明放在文件区下面，顶部只留来源、进度与出错 -->
        <p v-if="canScanFree" id="undelete-free-hint" class="wizard__visually-hidden">{{ t('partitionFiles.scan.free.hint') }}</p>
        <p v-if="isSourceDisk" class="wizard__hint">{{ t('partitionFiles.browse.liveHint') }}</p>
      </div>
    </form>
  </section>
</template>

<style scoped>
.undelete__top {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-xs) var(--space-md);
}

/* 来源与分区先收缩换行，按钮留在右端 */
.undelete__top > dl {
  flex: 1 1 0;
  min-width: 0;
}
</style>
