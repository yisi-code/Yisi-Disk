<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { useEngineCapabilities } from '../composables/useEngineCapabilities'
import {
  FEATURE_METHODS,
  PROBLEMS,
  type FeatureKey,
  type FeatureView,
  type FocusRequest,
  type NavPage,
} from '../navigation'

const props = defineProps<{ focusRequest: FocusRequest | null }>()
const emit = defineEmits<{ navigate: [page: NavPage]; open: [view: FeatureView, feature: FeatureKey] }>()

const { t } = useI18n()
const engine = useEngineCapabilities()

const titleElement = ref<HTMLHeadingElement | null>(null)
const surveyElement = ref<HTMLButtonElement | null>(null)

/** 引擎不支持诊断时不给这个入口，按问题选照常可用 */
const canSurvey = computed(() => engine.failure.value === null && engine.supports(FEATURE_METHODS.survey))

onMounted(() => {
  void engine.load()
  if (props.focusRequest?.target === 'title') {
    titleElement.value?.focus()
  } else if (props.focusRequest?.target === 'feature' && props.focusRequest.feature === 'survey') {
    surveyElement.value?.focus()
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

    <button
      v-if="canSurvey"
      ref="surveyElement"
      class="nav-card problem-card problem-card--survey"
      type="button"
      @click="emit('open', 'survey', 'survey')"
    >
      <span class="problem-card__name">{{ t('problems.overview.survey.name') }}</span>
      <span class="problem-card__summary">{{ t('problems.overview.survey.summary') }}</span>
    </button>

    <ul class="guide-page__cards guide-page__cards--grid">
      <li v-for="problem in PROBLEMS" :key="problem.key">
        <button class="nav-card problem-card" type="button" @click="emit('navigate', problem.key)">
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
.problem-card__name {
  font-size: var(--font-size-section-title);
  font-weight: 600;
}

.problem-card__summary {
  color: var(--color-text-muted);
}

/* 不确定是哪种问题时的入口：占满一行，左侧色条与问题卡片区分开 */
.problem-card--survey {
  border-left: 4px solid var(--color-accent);
}
</style>
