<script setup lang="ts">
import { useI18n } from 'vue-i18n'
import { SUPPORTED_LOCALES, saveLocale, type AppLocale } from '../i18n'

/** 设置：界面语言（选了就记住）。以后其他全程序的设置也放这里 */
const { t, locale } = useI18n()

function changeLocale(event: Event) {
  const next = (event.target as HTMLSelectElement).value as AppLocale
  locale.value = next
  saveLocale(next)
}
</script>

<template>
  <section class="guide-page" aria-labelledby="settings-title">
    <header class="guide-page__header">
      <h2 id="settings-title" class="guide-page__title">{{ t('settings.title') }}</h2>
      <p class="guide-page__intro">{{ t('settings.intro') }}</p>
    </header>

    <dl class="settings__list">
      <dt>
        <label for="settings-locale">{{ t('locale.label') }}</label>
      </dt>
      <dd>
        <select id="settings-locale" class="settings__select" :value="locale" @change="changeLocale">
          <option v-for="code in SUPPORTED_LOCALES" :key="code" :value="code">
            {{ t(`locale.${code}`) }}
          </option>
        </select>
        <p class="settings__hint">{{ t('settings.localeHint') }}</p>
      </dd>
    </dl>
  </section>
</template>

<style scoped>
.settings__list {
  display: grid;
  grid-template-columns: max-content minmax(0, 1fr);
  gap: var(--space-sm) var(--space-lg);
  margin: 0;
}

.settings__list dt {
  padding-top: var(--space-xs);
  color: var(--color-text-muted);
}

.settings__list dd {
  margin: 0;
}

.settings__select {
  padding: var(--space-xs) var(--space-sm);
  color: var(--color-text);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-control);
  font: inherit;
}

.settings__hint {
  margin: var(--space-xs) 0 0;
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
}
</style>
