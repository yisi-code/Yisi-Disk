<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { useHostErrorText } from '../composables/useHostErrorText'
import { listDisks, toHostError, type DiskSummary, type HostError } from '../services/host'
import { formatBytes } from '../utils/format'

defineProps<{ disabled: boolean }>()
const selectedDisk = defineModel<number | null>({ required: true })

const { t, te } = useI18n()
const { formatHostError } = useHostErrorText()

const disks = ref<DiskSummary[]>([])
const isLoading = ref(false)
const loadError = ref<HostError | null>(null)

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

function diskName(disk: DiskSummary): string {
  const name = [disk.vendor, disk.model].filter((part) => part !== '').join(' ')
  return name === '' ? t('disk.unnamed') : name
}

function busLabel(disk: DiskSummary): string {
  const key = `disk.bus.${disk.bus}`
  return te(key) ? t(key) : disk.bus
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

    <ul v-if="disks.length > 0" class="disk-picker__list">
      <li v-for="disk in disks" :key="disk.number">
        <label class="wizard__choice">
          <input
            v-model="selectedDisk"
            class="disk-picker__radio"
            type="radio"
            name="wizard__choices"
            :value="disk.number"
          />
          <span class="disk-picker__name">
            {{ t('disk.title', { number: disk.number, name: diskName(disk) }) }}
          </span>
          <span class="disk-picker__meta">
            {{
              t('disk.meta', {
                bus: busLabel(disk),
                size: formatBytes(disk.size),
                sectorSize: disk.sectorSize,
              })
            }}
            <template v-if="disk.removable"> · {{ t('disk.removable') }}</template>
          </span>
        </label>
      </li>
    </ul>

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

.disk-picker__list {
  margin: 0;
  padding: 0;
  list-style: none;
}

.disk-picker__radio {
  align-self: center;
  accent-color: var(--color-accent);
}

.disk-picker__name {
  font-weight: 600;
}

.disk-picker__meta {
  color: var(--color-text-muted);
}

.disk-picker__refresh {
  margin-top: var(--space-xs);
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
