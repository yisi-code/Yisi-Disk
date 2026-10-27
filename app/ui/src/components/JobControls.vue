<script setup lang="ts">
import { useI18n } from 'vue-i18n'
import type { JobStatus } from '../composables/useJob'

/**
 * 任务进行中的按钮：可以暂停的任务先是「暂停 / 继续」，然后是「取消」（取消中显示「正在取消…」）。
 * 两个按钮直接排进使用方的按钮行（`.wizard__nav` 等）。各任务页共用
 */
const props = defineProps<{
  status: JobStatus
  /** 给了就显示暂停 / 继续 */
  pause?: { isPaused: boolean; isToggling: boolean }
}>()
const emit = defineEmits<{ pause: []; resume: []; cancel: [] }>()

const { t } = useI18n()

function togglePause() {
  if (props.pause?.isPaused) {
    emit('resume')
  } else {
    emit('pause')
  }
}
</script>

<template>
  <button
    v-if="pause !== undefined && status === 'running'"
    class="wizard__button wizard__button--secondary"
    type="button"
    :disabled="pause.isToggling"
    @click="togglePause"
  >
    {{ pause.isPaused ? t('common.resume') : t('common.pause') }}
  </button>
  <button
    class="wizard__button wizard__button--secondary"
    type="button"
    :disabled="status !== 'running'"
    @click="emit('cancel')"
  >
    {{ status === 'cancelling' ? t('common.cancelling') : t('common.cancel') }}
  </button>
</template>
