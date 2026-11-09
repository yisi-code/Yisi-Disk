<script setup lang="ts">
import { computed, onMounted } from 'vue'
import { useI18n } from 'vue-i18n'
import { useEngineCapabilities } from '../composables/useEngineCapabilities'
import { injectFeatureJump } from '../composables/useFeatureJump'
import { FEATURE_METHODS, FEATURE_VIEWS, type FeatureKey } from '../navigation'
import type { JobTarget, PartitionRef, SearchMode } from '../services/host'

/** 能直接打开某个分区的功能；其余功能只带源过去（例如深度恢复扫的是整个源） */
const PARTITION_FEATURES: FeatureKey[] = ['undelete', 'fsCheck']

/**
 * 转到别的功能的一排按钮：带着同一个源（给了 partition 时，能按分区打开的功能还带着同一个分区）直接进入，不必回首页重新选。
 * 所在的功能页没有提供转去的入口（useFeatureJump）、引擎不支持的功能都不显示；一个都没有时整排不显示。
 * lead 是按钮前的一句引导
 */
const props = defineProps<{
  features: FeatureKey[]
  target: JobTarget | null
  partition?: PartitionRef | null
  /** 转到找回丢失的分区时怎么搜 */
  searchMode?: SearchMode
  lead?: string
  disabled?: boolean
}>()

const { t } = useI18n()
const jump = injectFeatureJump()
const engine = useEngineCapabilities()

const available = computed(() =>
  jump === null || props.target === null
    ? []
    : props.features.filter(
        (feature) => FEATURE_VIEWS[feature] !== null && engine.supports(FEATURE_METHODS[feature]),
      ),
)

onMounted(() => {
  void engine.load()
})

function partitionFor(feature: FeatureKey): PartitionRef | null {
  return PARTITION_FEATURES.includes(feature) ? (props.partition ?? null) : null
}

function label(feature: FeatureKey): string {
  const name = t(`features.${feature}.name`)
  return partitionFor(feature) ? t('jump.toPartition', { name }) : t('jump.to', { name })
}

function go(feature: FeatureKey) {
  if (jump === null || props.target === null || props.disabled) {
    return
  }
  void jump({
    feature,
    target: props.target,
    partition: partitionFor(feature),
    searchMode: props.searchMode,
  })
}
</script>

<template>
  <div v-if="available.length > 0" class="wizard__actions">
    <span v-if="lead" class="wizard__note">{{ lead }}</span>
    <button
      v-for="feature in available"
      :key="feature"
      class="wizard__button wizard__button--secondary"
      type="button"
      :disabled="disabled"
      @click="go(feature)"
    >
      {{ label(feature) }}
    </button>
  </div>
</template>
