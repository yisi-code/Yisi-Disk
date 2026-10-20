<script setup lang="ts">
import { onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { SUPPORTED_LOCALES } from './i18n'
import { getAppInfo } from './services/host'

const SCENARIO_KEYS = [
  'findPartitions',
  'repairPartition',
  'undelete',
  'carve',
  'fsCheck',
  'image',
] as const

const { t, locale } = useI18n()

const appVersion = ref<string | null>(null)
const hostError = ref<string | null>(null)

watch(
  locale,
  (currentLocale) => {
    document.documentElement.lang = currentLocale
  },
  { immediate: true },
)

onMounted(async () => {
  try {
    appVersion.value = (await getAppInfo()).version
  } catch (error) {
    hostError.value = String(error)
  }
})
</script>

<template>
  <div class="app-layout">
    <header class="app-header">
      <div class="app-header__brand">
        <h1 class="app-header__title">{{ t('app.name') }}</h1>
        <span class="app-header__subtitle">{{ t('app.subtitle') }}</span>
      </div>
      <label class="locale-picker">
        <span class="locale-picker__label">{{ t('locale.label') }}</span>
        <select v-model="locale" class="locale-picker__select">
          <option v-for="code in SUPPORTED_LOCALES" :key="code" :value="code">
            {{ t(`locale.${code}`) }}
          </option>
        </select>
      </label>
    </header>

    <main class="app-main">
      <h2 class="scenario-section__title">{{ t('scenarios.title') }}</h2>
      <ul class="scenario-list">
        <li v-for="key in SCENARIO_KEYS" :key="key" class="scenario-card">
          <span class="scenario-card__name">{{ t(`scenarios.${key}`) }}</span>
          <span class="scenario-card__status">{{ t('scenarios.unavailable') }}</span>
        </li>
      </ul>
    </main>

    <footer class="app-footer">
      <span v-if="hostError" role="alert">{{ t('host.failed', { reason: hostError }) }}</span>
      <span v-else-if="appVersion">{{ t('host.version', { version: appVersion }) }}</span>
      <span v-else>{{ t('host.connecting') }}</span>
    </footer>
  </div>
</template>

<style scoped>
.app-layout {
  display: flex;
  flex-direction: column;
  height: 100%;
}

.app-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: var(--space-md) var(--space-lg);
  background: var(--color-surface);
  border-bottom: 1px solid var(--color-border);
}

.app-header__brand {
  display: flex;
  align-items: baseline;
  gap: var(--space-sm);
}

.app-header__title {
  margin: 0;
  font-size: var(--font-size-app-title);
  font-weight: 600;
}

.app-header__subtitle {
  color: var(--color-text-muted);
}

.locale-picker {
  display: flex;
  align-items: center;
  gap: var(--space-sm);
  color: var(--color-text-muted);
}

.locale-picker__select {
  padding: var(--space-xs) var(--space-sm);
  color: var(--color-text);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--space-xs);
  font: inherit;
}

.app-main {
  flex: 1;
  overflow: auto;
  padding: var(--space-lg);
}

.scenario-section__title {
  margin: 0 0 var(--space-md);
  font-size: var(--font-size-section-title);
  font-weight: 600;
}

.scenario-list {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: var(--space-md);
  margin: 0;
  padding: 0;
  list-style: none;
}

.scenario-card {
  display: flex;
  flex-direction: column;
  gap: var(--space-xs);
  padding: var(--space-md);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-card);
}

.scenario-card__name {
  font-weight: 600;
}

.scenario-card__status {
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
}

.app-footer {
  padding: var(--space-sm) var(--space-lg);
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
  background: var(--color-surface);
  border-top: 1px solid var(--color-border);
}
</style>
