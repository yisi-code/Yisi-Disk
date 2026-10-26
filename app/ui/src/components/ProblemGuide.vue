<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import FeatureCard from './FeatureCard.vue'
import {
  PROBLEMS,
  type FeatureKey,
  type FeatureView,
  type FocusRequest,
  type ProblemKey,
} from '../navigation'

const props = defineProps<{ problem: ProblemKey; focusRequest: FocusRequest | null }>()
const emit = defineEmits<{ open: [view: FeatureView, feature: FeatureKey] }>()

const { t, tm, rt } = useI18n()

const titleElement = ref<HTMLHeadingElement | null>(null)

const features = computed(
  () => PROBLEMS.find((candidate) => candidate.key === props.problem)?.features ?? [],
)
const symptoms = computed(() => messageList(`problems.${props.problem}.symptoms`))
const notes = computed(() => messageList(`problems.${props.problem}.notes`))

onMounted(() => {
  if (props.focusRequest?.target === 'title') {
    titleElement.value?.focus()
  }
})

/** 症状与注意事项是字符串数组，条数随语言可以不同 */
function messageList(key: string): string[] {
  return (tm(key) as string[]).map((item) => rt(item))
}

function shouldFocus(feature: FeatureKey): boolean {
  return props.focusRequest?.target === 'feature' && props.focusRequest.feature === feature
}
</script>

<template>
  <section class="guide-page" aria-labelledby="problem-title">
    <header class="guide-page__header">
      <h2 id="problem-title" ref="titleElement" class="guide-page__title" tabindex="-1">
        {{ t(`problems.${problem}.name`) }}
      </h2>
    </header>

    <section class="guide-page__section" aria-labelledby="problem-symptoms-title">
      <h3 id="problem-symptoms-title" class="guide-page__section-title">
        {{ t('problems.symptomsTitle') }}
      </h3>
      <ul class="guide-page__bullets">
        <li v-for="symptom in symptoms" :key="symptom">{{ symptom }}</li>
      </ul>
    </section>

    <section class="guide-page__section" aria-labelledby="problem-features-title">
      <h3 id="problem-features-title" class="guide-page__section-title">
        {{ t('problems.featuresTitle') }}
      </h3>
      <ol class="guide-page__cards">
        <li v-for="(feature, index) in features" :key="feature">
          <FeatureCard
            :feature="feature"
            :step="index + 1"
            :step-note="t(`problems.${problem}.steps.${feature}`)"
            :should-focus="shouldFocus(feature)"
            @open="(view) => emit('open', view, feature)"
          />
        </li>
      </ol>
    </section>

    <section class="guide-page__section" aria-labelledby="problem-notes-title">
      <h3 id="problem-notes-title" class="guide-page__section-title">
        {{ t('problems.notesTitle') }}
      </h3>
      <div class="guide-page__notice">
        <ul class="guide-page__bullets">
          <li v-for="note in notes" :key="note">{{ note }}</li>
        </ul>
      </div>
    </section>
  </section>
</template>
