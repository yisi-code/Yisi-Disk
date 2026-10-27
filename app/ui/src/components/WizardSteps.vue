<script setup lang="ts">
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'

/**
 * 向导顶上的步骤条：编号加名字，当前步骤高亮，之前的标为已完成。放在 `.wizard__stepbar` 里，
 * 右边是各向导自己的按钮。全部向导共用
 */
const props = defineProps<{
  steps: readonly string[]
  current: string
  /** 步骤名的 i18n 前缀，例如 `undelete.steps` */
  labelPrefix: string
}>()

const { t } = useI18n()
const currentIndex = computed(() => props.steps.indexOf(props.current))
</script>

<template>
  <ol class="wizard__steps">
    <li
      v-for="(name, index) in steps"
      :key="name"
      class="wizard__step"
      :class="{
        'wizard__step--current': name === current,
        'wizard__step--done': index < currentIndex,
      }"
      :aria-current="name === current ? 'step' : undefined"
    >
      <span class="wizard__step-number">{{ index + 1 }}</span>
      {{ t(`${labelPrefix}.${name}`) }}
    </li>
  </ol>
</template>
