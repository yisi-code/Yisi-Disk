<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { useHostErrorText } from '../composables/useHostErrorText'
import { usePartitionText } from '../composables/usePartitionText'
import { listDisks, toHostError, type DiskSummary, type HostError } from '../services/host'
import { formatBytes } from '../utils/format'

/**
 * 选物理磁盘：分「本地磁盘」与「外接磁盘」两组，每块盘一张卡片，只放主要信息（名称、容量、分区表格式、
 * 固态或机械、盘符与文件系统）；编号、总线、扇区大小、序列号、各卷已用多少收在卡片的「详情」里
 */
defineProps<{ disabled: boolean }>()
const selectedDisk = defineModel<number | null>({ required: true })

/** 经 USB、SD、MMC 接上的，或者系统报告为可移动的，算外接 */
const EXTERNAL_BUSES: DiskSummary['bus'][] = ['usb', 'sd', 'mmc']

const { t, te } = useI18n()
const { formatHostError } = useHostErrorText()
const { diskName, filesystemName } = usePartitionText()

const disks = ref<DiskSummary[]>([])
const isLoading = ref(false)
const loadError = ref<HostError | null>(null)

const groups = computed(() =>
  [
    { key: 'local', disks: disks.value.filter((disk) => !isExternal(disk)) },
    { key: 'external', disks: disks.value.filter(isExternal) },
  ].filter((group) => group.disks.length > 0),
)

onMounted(refreshDisks)

async function refreshDisks() {
  isLoading.value = true
  loadError.value = null
  try {
    const found = await listDisks()
    disks.value = found
    if (selectedDisk.value !== null && !found.some((disk) => disk.number === selectedDisk.value)) {
      selectedDisk.value = null
    }
  } catch (error) {
    loadError.value = toHostError(error)
  } finally {
    isLoading.value = false
  }
}

function isExternal(disk: DiskSummary): boolean {
  return disk.removable || EXTERNAL_BUSES.includes(disk.bus)
}

function busLabel(disk: DiskSummary): string {
  const key = `disk.bus.${disk.bus}`
  return te(key) ? t(key) : disk.bus
}

/** 卡片上的标签：分区表格式、固态或机械（查不到的不显示） */
function tags(disk: DiskSummary): string[] {
  const found: string[] = []
  if (disk.partitionStyle !== null) {
    found.push(t(`disk.style.${disk.partitionStyle}`))
  }
  if (disk.solidState !== null) {
    found.push(disk.solidState ? t('disk.solidState') : t('disk.spinning'))
  }
  return found
}

/** 「C:、D: NTFS · E: exFAT」：同一种文件系统的盘符写在一起，名字只出现一次；没有盘符时说明 */
function volumesLine(disk: DiskSummary): string {
  if (disk.volumes.length === 0) {
    return t('disk.noLetters')
  }
  const byFileSystem = new Map<string, string[]>()
  for (const volume of disk.volumes) {
    const letters = byFileSystem.get(volume.fileSystem) ?? []
    letters.push(`${volume.letter}:`)
    byFileSystem.set(volume.fileSystem, letters)
  }
  return [...byFileSystem]
    .map(([fileSystem, letters]) =>
      t('disk.volume', { letters: letters.join(t('disk.letterSeparator')), fileSystem: filesystemName(fileSystem) }),
    )
    .join(' · ')
}
</script>

<template>
  <fieldset class="wizard__choices" :disabled="disabled">
    <legend class="disk-picker__legend">{{ t('disk.legend') }}</legend>

    <p v-if="loadError" class="disk-picker__error" role="alert">
      {{ formatHostError(loadError) }}
    </p>
    <p v-else-if="isLoading && disks.length === 0" class="disk-picker__note">
      {{ t('disk.loading') }}
    </p>
    <p v-else-if="disks.length === 0" class="disk-picker__note">{{ t('disk.empty') }}</p>

    <section v-for="group in groups" :key="group.key" class="disk-picker__group">
      <h4 class="disk-picker__group-title">
        {{ t(`disk.group.${group.key}`, { count: group.disks.length }, group.disks.length) }}
      </h4>
      <ul class="disk-picker__list">
        <li
          v-for="disk in group.disks"
          :key="disk.number"
          class="disk-card"
          :class="{ 'disk-card--selected': selectedDisk === disk.number }"
        >
          <label class="disk-card__main">
            <input
              v-model="selectedDisk"
              class="disk-card__radio"
              type="radio"
              name="wizard__choices"
              :value="disk.number"
            />
            <span class="disk-card__text">
              <span class="disk-card__name">
                {{ t('disk.title', { number: disk.number, name: diskName(disk) }) }}
              </span>
              <span class="disk-card__meta">
                {{ formatBytes(disk.size) }}
                <span v-for="tag in tags(disk)" :key="tag" class="disk-card__tag">{{ tag }}</span>
              </span>
              <span class="disk-card__volumes">{{ volumesLine(disk) }}</span>
            </span>
          </label>
          <details class="disk-card__details">
            <summary class="disk-card__summary">{{ t('disk.details') }}</summary>
            <dl class="disk-card__facts">
              <dt>{{ t('disk.facts.bus') }}</dt>
              <dd>
                {{ busLabel(disk) }}<template v-if="disk.removable"> · {{ t('disk.removable') }}</template>
              </dd>
              <dt>{{ t('disk.facts.sector') }}</dt>
              <dd>{{ t('disk.facts.sectorValue', { size: disk.sectorSize }) }}</dd>
              <dt>{{ t('disk.facts.serial') }}</dt>
              <dd>{{ disk.serial || t('disk.facts.unknown') }}</dd>
              <template v-for="volume in disk.volumes" :key="volume.letter">
                <dt>{{ t('disk.facts.volume', { letter: volume.letter }) }}</dt>
                <dd>
                  {{
                    t('disk.facts.volumeValue', {
                      label: volume.label || t('disk.facts.noLabel'),
                      fileSystem: filesystemName(volume.fileSystem),
                      used: formatBytes(volume.totalBytes - volume.freeBytes),
                      total: formatBytes(volume.totalBytes),
                    })
                  }}
                  <meter
                    class="disk-card__usage"
                    min="0"
                    :max="volume.totalBytes || 1"
                    :value="volume.totalBytes - volume.freeBytes"
                  ></meter>
                </dd>
              </template>
            </dl>
          </details>
        </li>
      </ul>
    </section>

    <button
      class="disk-picker__refresh"
      type="button"
      :disabled="isLoading"
      @click="refreshDisks"
    >
      {{ t('disk.refresh') }}
    </button>
  </fieldset>
</template>

<style scoped>
.disk-picker__legend {
  padding: 0 var(--space-xs);
  color: var(--color-text-muted);
}

.disk-picker__note {
  margin: var(--space-xs) 0;
  color: var(--color-text-muted);
}

.disk-picker__error {
  margin: var(--space-xs) 0;
  color: var(--color-danger);
}

.disk-picker__group + .disk-picker__group {
  margin-top: var(--space-sm);
}

.disk-picker__group-title {
  margin: var(--space-xs) 0;
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
  font-weight: 600;
}

.disk-picker__list {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(280px, 1fr));
  gap: var(--space-sm);
  margin: 0;
  padding: 0;
  list-style: none;
}

.disk-card {
  display: flex;
  flex-direction: column;
  min-width: 0;
  padding: var(--space-sm);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-card);
}

/* 选中的除了边框颜色，单选框本身也是选中状态，不只靠颜色区分 */
.disk-card--selected {
  border-color: var(--color-accent);
  box-shadow: 0 0 0 1px var(--color-accent);
}

.disk-card__main {
  display: flex;
  align-items: flex-start;
  gap: var(--space-sm);
  cursor: pointer;
}

.disk-card__radio {
  margin-top: 4px;
  accent-color: var(--color-accent);
}

.disk-card__text {
  display: flex;
  flex-direction: column;
  min-width: 0;
}

.disk-card__name {
  font-weight: 600;
  overflow-wrap: anywhere;
}

.disk-card__meta,
.disk-card__volumes {
  color: var(--color-text-muted);
}

.disk-card__tag {
  margin-left: var(--space-xs);
  padding: 0 var(--space-xs);
  font-size: var(--font-size-small);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-control);
}

.disk-card__details {
  margin-top: var(--space-xs);
  padding-left: calc(var(--space-sm) + 13px);
}

.disk-card__summary {
  color: var(--color-accent);
  font-size: var(--font-size-small);
  cursor: pointer;
}

.disk-card__facts {
  display: grid;
  grid-template-columns: max-content 1fr;
  gap: 2px var(--space-sm);
  margin: var(--space-xs) 0 0;
  font-size: var(--font-size-small);
}

.disk-card__facts dt {
  color: var(--color-text-muted);
}

.disk-card__facts dd {
  margin: 0;
  overflow-wrap: anywhere;
}

.disk-card__usage {
  display: block;
  width: 100%;
  height: 6px;
}

.disk-picker__refresh {
  margin-top: var(--space-sm);
  padding: var(--space-xs) var(--space-sm);
  color: var(--color-text);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-control);
  font: inherit;
  cursor: pointer;
}

.disk-picker__refresh:disabled {
  opacity: 0.5;
  cursor: default;
}
</style>
