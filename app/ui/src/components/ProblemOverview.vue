<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { PROBLEMS, type FocusRequest, type NavPage } from '../navigation'

const props = defineProps<{ focusRequest: FocusRequest | null }>()
const emit = defineEmits<{ navigate: [page: NavPage] }>()

const { t } = useI18n()

const titleElement = ref<HTMLHeadingElement | null>(null)

onMounted(() => {
  if (props.focusRequest?.target === 'title') {
    titleElement.value?.focus()
  }
})
</script>

<template>
  <section class="guide-page" aria-labelledby="overview-title">
    <header class="guide-page__header">
      <h2 id="overview-title" ref="titleElement" class="guide-page__title" tabindex="-1">
        {{ t('problems.overview.title') }}
      </h2>
      <p class="guide-page__intro">{{ t('problems.overview.intro') }}</p>
    </header>

    <ul class="guide-page__cards guide-page__cards--grid">
      <li v-for="problem in PROBLEMS" :key="problem.key">
        <button class="problem-card" type="button" @click="emit('navigate', problem.key)">
          <span class="problem-card__name">{{ t(`problems.${problem.key}.name`) }}</span>
          <span class="problem-card__summary">{{ t(`problems.${problem.key}.summary`) }}</span>
        </button>
      </li>
    </ul>

    <p class="guide-page__notice">{{ t('problems.overview.safety') }}</p>

    <p class="guide-page__intro">
      {{ t('problems.overview.allFeaturesHint') }}
      <button class="guide-page__link" type="button" @click="emit('navigate', 'allFeatures')">
        {{ t('problems.overview.allFeaturesLink') }}
      </button>
    </p>
  </section>
</template>

<style scoped>
.problem-card {
  display: flex;
  flex-direction: column;
  gap: var(--space-xs);
  width: 100%;
  padding: var(--space-md);
  color: var(--color-text);
  font: inherit;
  text-align: left;
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-card);
  cursor: pointer;
}

.problem-card:hover {
  border-color: var(--color-accent);
}

.problem-card__name {
  font-size: var(--font-size-section-title);
  font-weight: 600;
}

.problem-card__summary {
  color: var(--color-text-muted);
}
</style>
