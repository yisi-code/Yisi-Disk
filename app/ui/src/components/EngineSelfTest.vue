<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { useReadScan } from '../composables/useReadScan'
import {
  getEngineCapabilities,
  toHostError,
  type EngineCapabilities,
  type HostError,
} from '../services/host'

const BYTE_UNITS = ['B', 'KiB', 'MiB', 'GiB', 'TiB'] as const

const { t, te } = useI18n()
const {
  status,
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
const imagePath = ref('')

const canStart = computed(
  () => capabilities.value !== null && imagePath.value.trim() !== '' && !isBusy.value,
)

const statusText = computed(() => {
  switch (status.value) {
    case 'idle':
      return ''
    case 'starting':
      return t('selfTest.status.starting')
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
  if (canStart.value) {
    void start(imagePath.value.trim())
  }
}

function handleCancel() {
  void cancel()
}

function formatBytes(bytes: number): string {
  let value = bytes
  let unitIndex = 0
  while (value >= 1024 && unitIndex < BYTE_UNITS.length - 1) {
    value /= 1024
    unitIndex += 1
  }
  return `${value.toFixed(unitIndex === 0 ? 0 : 1)} ${BYTE_UNITS[unitIndex]}`
}

function formatError(error: HostError): string {
  const key = `errors.${error.code}`
  const message = te(key) ? t(key) : t('errors.unknown')
  return error.detail === '' ? message : t('errors.withDetail', { message, detail: error.detail })
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
        {{ formatError(capabilitiesError) }}
      </span>
      <template v-else>{{ t('selfTest.engineChecking') }}</template>
    </p>

    <form class="self-test__form" @submit.prevent="handleSubmit">
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
      <button v-if="!isBusy" class="self-test__button" type="submit" :disabled="!canStart">
        {{ t('selfTest.start') }}
      </button>
      <button
        v-else
        class="self-test__button self-test__button--secondary"
        type="button"
        :disabled="status === 'cancelling'"
        @click="handleCancel"
      >
        {{ status === 'cancelling' ? t('selfTest.cancelling') : t('selfTest.cancel') }}
      </button>
    </form>

    <div v-if="status !== 'idle'" class="self-test__job" aria-live="polite">
      <progress class="self-test__progress" :value="done" :max="total || 1"></progress>
      <p class="self-test__status">{{ statusText }}</p>
      <p v-if="isUnresponsive" class="self-test__error" role="alert">
        {{ t('selfTest.unresponsive') }}
      </p>
      <p v-if="failure" class="self-test__error" role="alert">{{ formatError(failure) }}</p>
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
        <dd>{{ t('selfTest.result.elapsedValue', { seconds: (report.elapsedMs / 1000).toFixed(1) }) }}</dd>
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
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: var(--space-xs);
  font: inherit;
}

.self-test__button {
  flex: none;
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
