<script setup lang="ts">
import { useI18n } from 'vue-i18n'

/** 最多列出这么多个，其余只算在总数里 */
const MAX_LISTED = 20

/**
 * 没能保存或复制的文件：一行总数（`countKey`，按数量取单复数），下面列出前 20 个的路径与原因。
 * 深度恢复的另存与分区文件的复制共用
 */
defineProps<{
  failures: { path: string; error: string | null }[]
  countKey: string
}>()

const { t } = useI18n()
</script>

<template>
  <template v-if="failures.length > 0">
    <p class="wizard__error">
      {{ t(countKey, { count: failures.length }, failures.length) }}
    </p>
    <ul class="wizard__failures">
      <li v-for="failed in failures.slice(0, MAX_LISTED)" :key="failed.path">
        {{ t('common.failedItem', { path: failed.path, reason: failed.error ?? '' }) }}
      </li>
    </ul>
  </template>
</template>
