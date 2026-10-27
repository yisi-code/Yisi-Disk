<script setup lang="ts">
import { computed, onMounted, ref, useId } from 'vue'
import { useI18n } from 'vue-i18n'
import { FEATURE_VIEWS, type FeatureKey, type FeatureView } from '../navigation'

const props = defineProps<{
  feature: FeatureKey
  /** 问题页里的推荐顺序，从 1 起；「全部功能」页不给 */
  step?: number
  /** 针对当前问题的一句说明 */
  stepNote?: string
  /** 挂载后把焦点放到这张卡片上（从功能页返回时） */
  shouldFocus?: boolean
}>()
const emit = defineEmits<{ open: [view: FeatureView] }>()

const { t } = useI18n()
const id = useId()

const cardElement = ref<HTMLButtonElement | null>(null)

const view = computed(() => FEATURE_VIEWS[props.feature])
const describedBy = computed(() =>
  [
    view.value === null ? `${id}-reason` : null,
    props.stepNote ? `${id}-note` : null,
    `${id}-summary`,
  ]
    .filter((part) => part !== null)
    .join(' '),
)

onMounted(() => {
  if (props.shouldFocus) {
    cardElement.value?.focus()
  }
})

/** 开发中的功能可以聚焦、能读到原因，但点了不进入 */
function handleClick() {
  if (view.value !== null) {
    emit('open', view.value)
  }
}
</script>

<template>
  <button
    ref="cardElement"
    class="nav-card feature-card"
    :class="{ 'feature-card--pending': view === null }"
    type="button"
    :aria-labelledby="`${id}-name`"
    :aria-describedby="describedBy"
    :aria-disabled="view === null ? 'true' : undefined"
    @click="handleClick"
  >
    <span class="feature-card__head">
      <span v-if="step !== undefined" class="feature-card__step">
        {{ t('problems.step', { step }) }}
      </span>
      <span :id="`${id}-name`" class="feature-card__name">{{ t(`features.${feature}.name`) }}</span>
      <span v-if="view === null" class="feature-card__badge" aria-hidden="true">
        {{ t('features.pending') }}
      </span>
    </span>
    <span v-if="stepNote" :id="`${id}-note`" class="feature-card__note">{{ stepNote }}</span>
    <span :id="`${id}-summary`" class="feature-card__summary">
      {{ t(`features.${feature}.summary`) }}
    </span>
    <span class="feature-card__fit">
      <span class="feature-card__fit-label">{{ t('features.suits') }}</span>
      {{ t(`features.${feature}.suits`) }}
    </span>
    <span class="feature-card__fit">
      <span class="feature-card__fit-label">{{ t('features.notSuits') }}</span>
      {{ t(`features.${feature}.notSuits`) }}
    </span>
    <span v-if="view === null" :id="`${id}-reason`" class="feature-card__pending-reason">
      {{ t('features.pendingReason') }}
    </span>
  </button>
</template>

<style scoped>
.feature-card--pending {
  background: var(--color-bg);
  border-style: dashed;
  cursor: default;
}

.feature-card--pending:hover {
  border-color: var(--color-border);
}

.feature-card--pending .feature-card__name,
.feature-card--pending .feature-card__note,
.feature-card--pending .feature-card__summary,
.feature-card--pending .feature-card__fit {
  color: var(--color-text-muted);
}

.feature-card__head {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-sm);
}

.feature-card__step {
  padding: 0 var(--space-sm);
  color: var(--color-on-accent);
  font-size: var(--font-size-small);
  background: var(--color-accent);
  border-radius: var(--radius-card);
}

.feature-card--pending .feature-card__step {
  color: var(--color-text-muted);
  background: transparent;
  border: 1px solid var(--color-border);
}

.feature-card__name {
  font-weight: 600;
}

.feature-card__badge {
  padding: 0 var(--space-sm);
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-card);
}

.feature-card__note {
  font-weight: 600;
}

.feature-card__summary,
.feature-card__fit,
.feature-card__pending-reason {
  font-size: var(--font-size-small);
}

.feature-card__fit-label {
  color: var(--color-text-muted);
  font-weight: 600;
}

.feature-card__pending-reason {
  color: var(--color-text-muted);
  font-style: italic;
}
</style>
