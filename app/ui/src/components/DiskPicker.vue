<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { useHostErrorText } from '../composables/useHostErrorText'
import { usePartitionText } from '../composables/usePartitionText'
import type { DeviceTarget } from '../composables/useSourceSelection'
import { listDisks, toHostError, type DiskSummary, type HostError } from '../services/host'
import { formatBytes } from '../utils/format'

/**
 * 选物理磁盘或其中一个卷：分「本地磁盘」与「外接磁盘」两组，每块盘一张卡片。卡片头选整块盘（名称、容量、分区表格式、
 * 固态或机械），下面每个盘符一行，可以单独选（卷名、用量条，条下方是已用 / 总大小与文件系统）；接口、扇区大小、
 * 序列号收在「详情」里
 */
defineProps<{ disabled: boolean }>()
const selected = defineModel<DeviceTarget | null>({ required: true })

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
    // 选中的盘或盘符不在了（拔掉了），就不再选着它
    const current = selected.value
    const stillThere =
      current === null ||
      (current.kind === 'disk'
        ? found.some((disk) => disk.number === current.diskNumber)
        : found.some((disk) => disk.volumes.some((volume) => volume.letter === current.letter)))
    if (!stillThere) {
      selected.value = null
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

function isSelectedDisk(disk: DiskSummary): boolean {
  const current = selected.value
  return (
    (current?.kind === 'disk' && current.diskNumber === disk.number) ||
    (current?.kind === 'volume' && disk.volumes.some((volume) => volume.letter === current.letter))
  )
}

function busLabel(disk: DiskSummary): string {
  const key = `disk.bus.${disk.bus}`
  return te(key) ? t(key) : disk.bus
}

/** 卡片头的标签：分区表格式、固态或机械（查不到的不显示） */
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
    <p v-else class="disk-picker__note">{{ t('disk.chooseHint') }}</p>

    <section v-for="group in groups" :key="group.key" class="disk-picker__group">
      <h4 class="disk-picker__group-title">
        {{ t(`disk.group.${group.key}`, { count: group.disks.length }, group.disks.length) }}
      </h4>
      <ul class="disk-picker__list">
        <li
          v-for="disk in group.disks"
          :key="disk.number"
          class="disk-card"
          :class="{ 'disk-card--selected': isSelectedDisk(disk) }"
        >
          <label class="disk-card__main">
            <input
              v-model="selected"
              class="disk-card__radio"
              type="radio"
              name="wizard__choices"
              :value="{ kind: 'disk', diskNumber: disk.number }"
            />
            <span class="disk-card__text">
              <span class="disk-card__name">
                {{ t('disk.title', { number: disk.number, name: diskName(disk) }) }}
              </span>
              <span class="disk-card__meta">
                {{ t('disk.wholeDisk', { size: formatBytes(disk.size) }) }}
                <span v-for="tag in tags(disk)" :key="tag" class="disk-card__tag">{{ tag }}</span>
              </span>
            </span>
          </label>

          <ul v-if="disk.volumes.length > 0" class="disk-card__volumes">
            <li v-for="volume in disk.volumes" :key="volume.letter">
              <label class="volume-row">
                <input
                  v-model="selected"
                  class="disk-card__radio"
                  type="radio"
                  name="wizard__choices"
                  :value="{ kind: 'volume', letter: volume.letter }"
                />
                <span class="volume-row__body">
                  <span class="volume-row__name">
                    {{
                      t('disk.volumeName', {
                        label: volume.label || t('disk.volumeNoLabel'),
                        letter: volume.letter,
                      })
                    }}
                  </span>
                  <meter
                    class="volume-row__usage"
                    min="0"
                    :max="volume.totalBytes || 1"
                    :value="volume.totalBytes - volume.freeBytes"
                  ></meter>
                  <span class="volume-row__meta">
                    {{
                      t('disk.volumeUsage', {
                        used: formatBytes(volume.totalBytes - volume.freeBytes),
                        total: formatBytes(volume.totalBytes),
                        fileSystem: filesystemName(volume.fileSystem),
                      })
                    }}
                  </span>
                </span>
              </label>
            </li>
          </ul>
          <p v-else class="disk-card__none">{{ t('disk.noLetters') }}</p>

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
  font-size: var(--font-size-small);
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
  grid-template-columns: repeat(auto-fill, minmax(300px, 1fr));
  gap: var(--space-sm);
  margin: 0;
  padding: 0;
  list-style: none;
}

.disk-card {
  display: flex;
  flex-direction: column;
  gap: var(--space-xs);
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

.disk-card__main,
.volume-row {
  display: flex;
  align-items: flex-start;
  gap: var(--space-sm);
  cursor: pointer;
}

.disk-card__radio {
  flex: none;
  margin-top: 4px;
  accent-color: var(--color-accent);
}

.disk-card__text,
.volume-row__body {
  display: flex;
  flex: 1;
  flex-direction: column;
  min-width: 0;
}

.disk-card__name {
  font-weight: 600;
  overflow-wrap: anywhere;
}

.disk-card__meta,
.volume-row__meta,
.disk-card__none {
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
}

.disk-card__none {
  margin: 0;
}

.disk-card__tag {
  margin-left: var(--space-xs);
  padding: 0 var(--space-xs);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-control);
}

.disk-card__volumes {
  display: flex;
  flex-direction: column;
  gap: var(--space-xs);
  margin: 0;
  padding: var(--space-xs) 0 0;
  list-style: none;
  border-top: 1px solid var(--color-border);
}

.volume-row__usage {
  display: block;
  width: 100%;
  height: 8px;
  margin: 2px 0;
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
