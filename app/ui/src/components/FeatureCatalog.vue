<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import FeatureCard from './FeatureCard.vue'
import { ALL_FEATURES, type FeatureKey, type FeatureView, type FocusRequest } from '../navigation'

const props = defineProps<{ focusRequest: FocusRequest | null }>()
const emit = defineEmits<{ open: [view: FeatureView, feature: FeatureKey] }>()

const { t } = useI18n()

const titleElement = ref<HTMLHeadingElement | null>(null)

onMounted(() => {
  if (props.focusRequest?.target === 'title') {
    titleElement.value?.focus()
  }
})

function shouldFocus(feature: FeatureKey): boolean {
  return props.focusRequest?.target === 'feature' && props.focusRequest.feature === feature
}
</script>

<template>
  <section class="guide-page" aria-labelledby="catalog-title">
    <header class="guide-page__header">
      <h2 id="catalog-title" ref="titleElement" class="guide-page__title" tabindex="-1">
        {{ t('nav.allFeatures') }}
      </h2>
      <p class="guide-page__intro">{{ t('features.catalogIntro') }}</p>
    </header>

    <ul class="guide-page__cards guide-page__cards--grid">
      <li v-for="feature in ALL_FEATURES" :key="feature">
        <FeatureCard
          :feature="feature"
          :should-focus="shouldFocus(feature)"
          @open="(view) => emit('open', view, feature)"
        />
      </li>
    </ul>
  </section>
</template>
