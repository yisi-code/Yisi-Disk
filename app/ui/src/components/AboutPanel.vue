<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { useHostErrorText } from '../composables/useHostErrorText'
import {
  getAppInfo,
  getEngineCapabilities,
  toHostError,
  type EngineCapabilities,
  type HostError,
} from '../services/host'

const { t } = useI18n()
const { formatHostError } = useHostErrorText()

const appVersion = ref<string | null>(null)
const appError = ref<HostError | null>(null)
const capabilities = ref<EngineCapabilities | null>(null)
const engineError = ref<HostError | null>(null)

// 每次打开都现查一次：引擎是否可用以这一刻为准
onMounted(() => {
  void loadAppVersion()
  void loadEngineStatus()
})

async function loadAppVersion() {
  try {
    appVersion.value = (await getAppInfo()).version
  } catch (error) {
    appError.value = toHostError(error)
  }
}

async function loadEngineStatus() {
  try {
    capabilities.value = await getEngineCapabilities()
  } catch (error) {
    engineError.value = toHostError(error)
  }
}
</script>

<template>
  <section class="guide-page" aria-labelledby="about-title">
    <header class="guide-page__header">
      <h2 id="about-title" class="guide-page__title">{{ t('about.title') }}</h2>
      <p class="guide-page__intro">{{ t('about.intro') }}</p>
    </header>

    <dl class="about__facts">
      <dt>{{ t('about.version') }}</dt>
      <dd>
        <span v-if="appError" class="about__error" role="alert">{{ formatHostError(appError) }}</span>
        <template v-else>{{ appVersion ?? t('about.checking') }}</template>
      </dd>
      <dt>{{ t('about.engine') }}</dt>
      <dd aria-live="polite">
        <template v-if="capabilities">{{ t('about.engineReady') }}</template>
        <template v-else-if="engineError">
          <span class="about__error" role="alert">{{ t('about.engineFailed') }}</span>
          <span class="about__hint">{{ formatHostError(engineError) }}</span>
        </template>
        <template v-else>{{ t('about.checking') }}</template>
      </dd>
    </dl>

    <details v-if="capabilities" class="about__details">
      <summary class="about__summary">{{ t('about.technical') }}</summary>
      <dl class="about__facts">
        <dt>{{ t('about.engineVersion') }}</dt>
        <dd>{{ capabilities.version }}</dd>
        <dt>{{ t('about.engineMethods') }}</dt>
        <dd>
          <ul class="about__methods">
            <li v-for="method in capabilities.methods" :key="method">
              <code>{{ method }}</code>
            </li>
          </ul>
        </dd>
      </dl>
    </details>
  </section>
</template>

<style scoped>
.about__facts {
  display: grid;
  grid-template-columns: max-content minmax(0, 1fr);
  gap: var(--space-xs) var(--space-md);
  margin: 0;
}

.about__facts dt {
  color: var(--color-text-muted);
}

.about__facts dd {
  display: flex;
  flex-direction: column;
  margin: 0;
}

.about__error {
  color: var(--color-danger);
}

.about__hint {
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
  overflow-wrap: anywhere;
}

.about__details {
  padding: var(--space-sm) var(--space-md);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-card);
}

.about__summary {
  cursor: pointer;
}

.about__details[open] .about__summary {
  margin-bottom: var(--space-sm);
}

.about__methods {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-xs) var(--space-md);
  margin: 0;
  padding: 0;
  list-style: none;
}

.about__methods code {
  font-family: ui-monospace, 'Cascadia Mono', Consolas, monospace;
  font-size: var(--font-size-small);
}
</style>
