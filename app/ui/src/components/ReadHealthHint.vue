<script setup lang="ts">
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import type { ReadHealth } from '../composables/useReadHealth'
import { formatBytes } from '../utils/format'

/**
 * 读得慢时说明原因（useJob 的 readHealth）：有损坏时建议先做镜像，内容零散或盘本身慢时说明不是盘坏了。
 * 只在源是物理磁盘时显示（镜像文件读得慢与那块盘的好坏无关）。各任务页放在进度条下方。
 * `context` 为 badSectors 时（坏道检测）读到坏区是在意料之中，建议换一种说法
 */
const props = defineProps<{
  health: ReadHealth | null
  sourceIsDisk: boolean
  context: 'scan' | 'badSectors'
}>()

const { t } = useI18n()

const isDamage = computed(
  () => props.health?.kind === 'damaged' || props.health?.kind === 'stuck',
)

const message = computed(() => {
  const health = props.health
  if (health === null) {
    return ''
  }
  switch (health.kind) {
    case 'damaged':
      return t('readHealth.damaged', { errors: health.errors, slow: health.slowReads })
    case 'stuck':
      return t('readHealth.stuck', { seconds: health.stalledSeconds })
    case 'scattered':
      return t('readHealth.scattered', { size: formatBytes(health.averageReadBytes) })
    case 'slowDevice':
      return t('readHealth.slowDevice', { rate: formatBytes(health.bytesPerSecond) })
  }
  return ''
})
</script>

<template>
  <div
    v-if="health && sourceIsDisk"
    class="read-health"
    :class="{ 'read-health--damage': isDamage }"
    :role="isDamage ? 'alert' : 'status'"
  >
    <p class="read-health__text">{{ message }}</p>
    <p v-if="isDamage" class="read-health__text">
      {{ context === 'badSectors' ? t('readHealth.adviceBadSectors') : t('readHealth.adviceImage') }}
    </p>
  </div>
</template>

<style scoped>
.read-health {
  display: block;
  width: 100%;
  padding: var(--space-xs) var(--space-sm);
  border-left: 3px solid var(--color-border);
  background: var(--color-bg);
}

.read-health--damage {
  border-left-color: var(--color-warning);
}

.read-health__text {
  margin: 0;
  font-size: var(--font-size-small);
}

.read-health__text + .read-health__text {
  margin-top: var(--space-xs);
}
</style>
