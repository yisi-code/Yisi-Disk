<script setup lang="ts">
import { useI18n } from 'vue-i18n'
import { useDiskAccess } from '../composables/useDiskAccess'

/** 底栏：用户在 UAC 里取消了、或代理没能启动时，在任何页面都能看到并重新授权 */
const { t } = useI18n()
const { access, authorizing, needsRetry, authorize } = useDiskAccess()
</script>

<template>
  <span v-if="needsRetry" class="disk-access-notice" role="status" aria-live="polite">
    <span>{{ t(`diskAccess.notice.${access}`) }}</span>
    <button
      class="wizard__button wizard__button--secondary disk-access-notice__button"
      type="button"
      :disabled="authorizing"
      @click="authorize"
    >
      {{ authorizing ? t('diskAccess.authorizing') : t('diskAccess.retry') }}
    </button>
  </span>
</template>

<style scoped>
.disk-access-notice {
  display: inline-flex;
  align-items: center;
  gap: var(--space-sm);
  min-width: 0;
  color: var(--color-text);
}

/* 底栏只有一行小字：按钮跟着变小，不把底栏撑高 */
.disk-access-notice__button {
  padding: 0 var(--space-sm);
}
</style>
