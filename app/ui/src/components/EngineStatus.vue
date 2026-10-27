<script setup lang="ts">
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import { useEngineStatus, type EngineState } from '../composables/useEngineStatus'
import { useHostErrorText } from '../composables/useHostErrorText'

/**
 * 底栏的引擎状态：圆点加文字。运行中、启动中、正在停止、无响应时圆点闪烁，颜色分别是绿、蓝、黄、红；
 * 空闲是灰色、浏览会话开着是绿色、暂停是黄色、引擎不可用是红色，都不闪。颜色之外总有文字，
 * 系统设了「减少动态效果」时不闪
 */
const { t } = useI18n()
const { formatHostError } = useHostErrorText()
const { state, availabilityError } = useEngineStatus()

const BLINKING: EngineState[] = ['running', 'starting', 'cancelling', 'unresponsive']

const text = computed(() =>
  state.value === 'unavailable' && availabilityError.value !== null
    ? t('engineStatus.unavailableWithReason', { reason: formatHostError(availabilityError.value) })
    : t(`engineStatus.${state.value}`),
)
</script>

<template>
  <span class="engine-status" role="status" aria-live="polite">
    <span
      class="engine-status__dot"
      :class="[`engine-status__dot--${state}`, { 'engine-status__dot--blink': BLINKING.includes(state) }]"
      aria-hidden="true"
    ></span>
    <span>{{ text }}</span>
  </span>
</template>

<style scoped>
.engine-status {
  display: inline-flex;
  align-items: center;
  gap: var(--space-xs);
  min-width: 0;
}

.engine-status__dot {
  flex: none;
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: var(--color-text-muted);
}

.engine-status__dot--session,
.engine-status__dot--running {
  background: var(--color-success);
}

.engine-status__dot--starting {
  background: var(--color-accent);
}

.engine-status__dot--paused,
.engine-status__dot--cancelling {
  background: var(--color-warning);
}

.engine-status__dot--unresponsive,
.engine-status__dot--unavailable {
  background: var(--color-danger);
}

.engine-status__dot--blink {
  animation: engine-status-blink 1.2s ease-in-out infinite;
}

@keyframes engine-status-blink {
  50% {
    opacity: 0.25;
  }
}

@media (prefers-reduced-motion: reduce) {
  .engine-status__dot--blink {
    animation: none;
  }
}
</style>
