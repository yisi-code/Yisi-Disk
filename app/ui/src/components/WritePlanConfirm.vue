<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { useHostErrorText } from '../composables/useHostErrorText'
import { usePartitionText } from '../composables/usePartitionText'
import { useWritePlanText } from '../composables/useWritePlanText'
import type { HostError, PlanView } from '../services/host'
import { formatBytes } from '../utils/format'

/**
 * 确认一份写入计划：写到哪里、做什么、改写哪几段、备份存在哪。勾选「我已了解」才能写，默认焦点在「取消」上。
 * 修复引导扇区与回滚共用
 */
const props = defineProps<{
  plan: PlanView
  /** 源的扇区大小，位置按它换算成扇区号 */
  sectorSize: number
  busy: boolean
  failure: HostError | null
}>()
const emit = defineEmits<{ confirm: []; cancel: [] }>()

const { t, locale } = useI18n()
const { formatHostError } = useHostErrorText()
const { sourceName, filesystemName } = usePartitionText()
const { writeActionText } = useWritePlanText()

const isAcknowledged = ref(false)
const cancelButton = ref<HTMLButtonElement | null>(null)

const detail = computed(() => props.plan.bootSector)
const totalBytes = computed(() => props.plan.writes.reduce((sum, range) => sum + range.length, 0))

const whatText = computed(() => {
  const current = detail.value
  if (current === null) {
    return null
  }
  return props.plan.kind === 'rollback'
    ? t('writePlan.rollbackValue', { to: position(current.to), size: formatBytes(current.length) })
    : t('writePlan.copyValue', {
        from: position(current.from),
        to: position(current.to),
        size: formatBytes(current.length),
      })
})

onMounted(() => {
  cancelButton.value?.focus()
})

function position(offset: number): string {
  return t('writePlan.position', {
    sector: Math.floor(offset / props.sectorSize).toLocaleString(locale.value),
    offset: offset.toLocaleString(locale.value),
  })
}
</script>

<template>
  <section class="write-plan" aria-labelledby="write-plan-title">
    <h3 id="write-plan-title" class="write-plan__title">{{ writeActionText(plan) }}</h3>
    <dl class="wizard__details">
      <dt>{{ t('writePlan.target') }}</dt>
      <dd class="wizard__path-text">{{ sourceName(plan.target) }}</dd>
      <template v-if="detail">
        <dt>{{ t('writePlan.partition') }}</dt>
        <dd>
          {{ t('writePlan.partitionValue', { number: detail.partition, filesystem: filesystemName(detail.filesystem) }) }}
        </dd>
      </template>
      <template v-if="whatText">
        <dt>{{ t('writePlan.what') }}</dt>
        <dd>{{ whatText }}</dd>
      </template>
      <dt>{{ t('writePlan.writes') }}</dt>
      <dd>
        <ul class="wizard__list">
          <li v-for="range in plan.writes" :key="range.offset">
            {{ t('writePlan.range', { position: position(range.offset), size: formatBytes(range.length) }) }}
          </li>
        </ul>
      </dd>
      <dt>{{ t('writePlan.backup') }}</dt>
      <dd class="wizard__path-text">{{ plan.backupPath }}</dd>
    </dl>
    <ul class="wizard__list">
      <li>{{ t('writePlan.safety.verify') }}</li>
      <li>{{ t('writePlan.safety.readBack') }}</li>
      <!-- 回滚本身不再回滚，只说明备份照样存了 -->
      <li>{{ plan.kind === 'rollback' ? t('writePlan.safety.backupKept') : t('writePlan.safety.rollback') }}</li>
    </ul>
    <p class="wizard__hint wizard__hint--warning">{{ t('writePlan.warning') }}</p>
    <label class="wizard__choice">
      <input v-model="isAcknowledged" type="checkbox" :disabled="busy" />
      <span>{{ t('writePlan.acknowledge', { size: formatBytes(totalBytes) }) }}</span>
    </label>
    <p v-if="failure" class="wizard__error" role="alert">{{ formatHostError(failure) }}</p>
    <div class="write-plan__actions">
      <button
        ref="cancelButton"
        class="wizard__button wizard__button--secondary"
        type="button"
        :disabled="busy"
        @click="emit('cancel')"
      >
        {{ t('writePlan.cancel') }}
      </button>
      <button class="wizard__button" type="button" :disabled="!isAcknowledged || busy" @click="emit('confirm')">
        {{ busy ? t('writePlan.writing') : t('writePlan.confirm') }}
      </button>
    </div>
  </section>
</template>

<style scoped>
.write-plan {
  display: flex;
  flex-direction: column;
  gap: var(--space-sm);
  width: 100%;
}

.write-plan__title {
  margin: 0;
  font-size: var(--font-size-body);
  font-weight: 600;
}

.write-plan__actions {
  display: flex;
  gap: var(--space-sm);
}
</style>
