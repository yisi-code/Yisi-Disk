<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import DiskPicker from './DiskPicker.vue'
import { useHostErrorText } from '../composables/useHostErrorText'
import { useReadScan, type ScanTarget } from '../composables/useReadScan'
import {
  getEngineCapabilities,
  toHostError,
  type EngineCapabilities,
  type HostError,
} from '../services/host'
import { formatBytes } from '../utils/format'

const LIST_DISKS_METHOD = 'disk.list'

const { t } = useI18n()
const { formatHostError } = useHostErrorText()
const {
  status,
  targetKind,
  done,
  total,
  percent,
  bytesPerSecond,
  report,
  failure,
  isUnresponsive,
  isBusy,
  start,
  cancel,
} = useReadScan()

const capabilities = ref<EngineCapabilities | null>(null)
const capabilitiesError = ref<HostError | null>(null)
const sourceKind = ref<ScanTarget['kind']>('image')
const imagePath = ref('')
const selectedDisk = ref<number | null>(null)

const canScanDisks = computed(
  () => capabilities.value?.methods.includes(LIST_DISKS_METHOD) ?? false,
)

const scanTarget = computed<ScanTarget | null>(() => {
  if (sourceKind.value === 'image') {
    const path = imagePath.value.trim()
    return path === '' ? null : { kind: 'image', path }
  }
  return selectedDisk.value === null ? null : { kind: 'disk', diskNumber: selectedDisk.value }
})

const canStart = computed(
  () => capabilities.value !== null && scanTarget.value !== null && !isBusy.value,
)

const statusText = computed(() => {
  switch (status.value) {
    case 'idle':
      return ''
    case 'starting':
      return targetKind.value === 'disk'
        ? t('selfTest.status.startingDisk')
        : t('selfTest.status.starting')
    case 'running':
    case 'cancelling':
      return t('selfTest.status.running', {
        done: formatBytes(done.value),
        total: formatBytes(total.value),
        percent: percent.value,
        rate: bytesPerSecond.value === null ? '-' : formatBytes(bytesPerSecond.value),
      })
    case 'completed':
      return t('selfTest.status.completed')
    case 'failed':
      return t('selfTest.status.failed')
    case 'cancelled':
      return t('selfTest.status.cancelled')
  }
  return ''
})

onMounted(async () => {
  try {
    capabilities.value = await getEngineCapabilities()
  } catch (error) {
    capabilitiesError.value = toHostError(error)
  }
})

function handleSubmit() {
  if (canStart.value && scanTarget.value !== null) {
    void start(scanTarget.value)
  }
}

function handleCancel() {
  void cancel()
}
</script>

<template>
  <section class="self-test" aria-labelledby="self-test-title">
    <h2 id="self-test-title" class="self-test__title">{{ t('selfTest.title') }}</h2>

    <p class="self-test__engine">
      <template v-if="capabilities">
        {{
          t('selfTest.engineReady', {
            version: capabilities.version,
            methods: capabilities.methods.join(', '),
          })
        }}
      </template>
      <span v-else-if="capabilitiesError" class="self-test__error" role="alert">
        {{ formatHostError(capabilitiesError) }}
      </span>
      <template v-else>{{ t('selfTest.engineChecking') }}</template>
    </p>

    <form class="self-test__form" @submit.prevent="handleSubmit">
      <fieldset class="self-test__source" :disabled="isBusy">
        <legend class="self-test__source-legend">{{ t('selfTest.source.legend') }}</legend>
        <label class="self-test__source-option">
          <input v-model="sourceKind" type="radio" name="self-test-source" value="image" />
          {{ t('selfTest.source.image') }}
        </label>
        <label class="self-test__source-option">
          <input
            v-model="sourceKind"
            type="radio"
            name="self-test-source"
            value="disk"
            :disabled="!canScanDisks"
          />
          {{ t('selfTest.source.disk') }}
        </label>
      </fieldset>

      <div v-if="sourceKind === 'image'" class="self-test__image">
        <label class="self-test__label" for="self-test-path">{{ t('selfTest.imagePath') }}</label>
        <input
          id="self-test-path"
          v-model="imagePath"
          class="self-test__input"
          type="text"
          spellcheck="false"
          autocomplete="off"
          :placeholder="t('selfTest.imagePathPlaceholder')"
          :disabled="isBusy"
        />
      </div>
      <template v-else>
        <DiskPicker v-model="selectedDisk" :disabled="isBusy" />
        <p class="self-test__hint">{{ t('selfTest.disk.hint') }}</p>
      </template>

      <div class="self-test__actions">
        <button v-if="!isBusy" class="self-test__button" type="submit" :disabled="!canStart">
          {{ t('selfTest.start') }}
        </button>
        <button
          v-else
          class="self-test__button self-test__button--secondary"
          type="button"
          :disabled="status !== 'running'"
          @click="handleCancel"
        >
          {{ status === 'cancelling' ? t('selfTest.cancelling') : t('selfTest.cancel') }}
        </button>
      </div>
    </form>

    <div v-if="status !== 'idle'" class="self-test__job" aria-live="polite">
      <progress class="self-test__progress" :value="done" :max="total || 1"></progress>
      <p class="self-test__status">{{ statusText }}</p>
      <p v-if="isUnresponsive" class="self-test__error" role="alert">
        {{ t('selfTest.unresponsive') }}
      </p>
      <p v-if="failure" class="self-test__error" role="alert">{{ formatHostError(failure) }}</p>
      <dl v-if="report && status === 'completed'" class="self-test__report">
        <dt>{{ t('selfTest.result.bytesRead') }}</dt>
        <dd>{{ formatBytes(report.bytesRead) }} / {{ formatBytes(report.size) }}</dd>
        <dt>{{ t('selfTest.result.unreadable') }}</dt>
        <dd>
          {{
            t('selfTest.result.unreadableValue', {
              count: report.unreadable.length,
              bytes: formatBytes(report.unreadableBytes),
            })
          }}
        </dd>
        <dt>{{ t('selfTest.result.elapsed') }}</dt>
        <dd>
          {{ t('selfTest.result.elapsedValue', { seconds: (report.elapsedMs / 1000).toFixed(1) }) }}
        </dd>
      </dl>
    </div>
  </section>
</template>

<style scoped>
.self-test {
  display: block;
  width: 100%;
  margin-top: var(--space-lg);
  padding: var(--space-md);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-card);
}

.self-test__title {
  margin: 0 0 var(--space-sm);
  font-size: var(--font-size-section-title);
  font-weight: 600;
}

.self-test__engine {
  margin: 0 0 var(--space-md);
  color: var(--color-text-muted);
}

.self-test__form {
  display: flex;
  flex-direction: column;
  gap: var(--space-sm);
}

.self-test__source {
  display: flex;
  gap: var(--space-md);
  margin: 0;
  padding: 0;
  border: none;
}

.self-test__source-legend {
  float: left;
  margin-right: var(--space-sm);
  padding: 0;
}

.self-test__source-option {
  display: flex;
  align-items: center;
  gap: var(--space-xs);
  cursor: pointer;
}

.self-test__image {
  display: flex;
  align-items: center;
  gap: var(--space-sm);
}

.self-test__label {
  flex: none;
}

.self-test__input {
  flex: 1;
  min-width: 0;
  padding: var(--space-xs) var(--space-sm);
  color: var(--color-text);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--space-xs);
  font: inherit;
}

.self-test__input:disabled {
  background: var(--color-bg);
}

.self-test__hint {
  margin: 0;
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
}

.self-test__actions {
  display: flex;
  justify-content: flex-end;
}

.self-test__button {
  padding: var(--space-xs) var(--space-md);
  color: var(--color-on-accent);
  background: var(--color-accent);
  border: 1px solid var(--color-accent);
  border-radius: var(--space-xs);
  font: inherit;
  cursor: pointer;
}

.self-test__button--secondary {
  color: var(--color-text);
  background: var(--color-surface);
  border-color: var(--color-border);
}

.self-test__button:disabled {
  opacity: 0.5;
  cursor: default;
}

.self-test__job {
  margin-top: var(--space-md);
}

.self-test__progress {
  display: block;
  width: 100%;
  height: var(--space-sm);
  accent-color: var(--color-accent);
}

.self-test__status {
  margin: var(--space-sm) 0 0;
}

.self-test__error {
  margin: var(--space-sm) 0 0;
  color: var(--color-danger);
}

.self-test__report {
  display: grid;
  grid-template-columns: max-content 1fr;
  gap: var(--space-xs) var(--space-md);
  margin: var(--space-sm) 0 0;
}

.self-test__report dt {
  color: var(--color-text-muted);
}

.self-test__report dd {
  margin: 0;
}
</style>
