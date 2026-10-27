<script setup lang="ts">
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import type { useBrowse } from '../composables/useBrowse'
import { formatBytes } from '../utils/format'

/**
 * 打开 NTFS 分区时扫描 MFT 的进度：进度条、已扫描多少、取消扫描。找回删除的文件与找回丢失的分区共用
 */
const props = defineProps<{
  browse: ReturnType<typeof useBrowse>
}>()

const { t } = useI18n()

const statusText = computed(() =>
  props.browse.scanTotal.value > 0
    ? t('partitionFiles.scan.running', {
        done: formatBytes(props.browse.scanDone.value),
        total: formatBytes(props.browse.scanTotal.value),
        percent: props.browse.scanPercent.value,
      })
    : t('partitionFiles.scan.starting'),
)
</script>

<template>
  <div class="wizard__job">
    <progress
      class="wizard__progress"
      :value="browse.scanDone.value"
      :max="browse.scanTotal.value || 1"
    ></progress>
    <p class="wizard__status">{{ statusText }}</p>
    <div>
      <button
        class="wizard__button wizard__button--secondary"
        type="button"
        :disabled="browse.isCancellingScan.value"
        @click="browse.stopScan"
      >
        {{ browse.isCancellingScan.value ? t('common.cancelling') : t('partitionFiles.scan.cancel') }}
      </button>
    </div>
  </div>
</template>
