<script setup lang="ts">
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import ReadHealthHint from './ReadHealthHint.vue'
import type { useBrowse } from '../composables/useBrowse'
import { isDeviceKind } from '../services/host'

/**
 * 浏览会话里扫描已删除项的进度：进度条、已扫描多少、读得慢的原因、取消扫描。两种扫描共用：NTFS 扫主文件表（MFT），
 * FAT / exFAT 读空闲簇（找原来的目录已找不到的文件夹），文字按 browse.scanKind 选。找回删除的文件与找回丢失的分区共用
 */
const props = defineProps<{
  browse: ReturnType<typeof useBrowse>
}>()

const { t } = useI18n()

/** 读空闲簇的文案在 partitionFiles.scan.free 下，扫 MFT 的直接在 partitionFiles.scan 下 */
const textPrefix = computed(() =>
  props.browse.scanKind.value === 'free' ? 'partitionFiles.scan.free' : 'partitionFiles.scan',
)

const statusText = computed(() =>
  props.browse.scanTotal.value > 0
    ? t(`${textPrefix.value}.running`, props.browse.scanProgressParams.value)
    : t(`${textPrefix.value}.starting`),
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
    <ReadHealthHint
      :health="browse.readHealth.value"
      :source-is-disk="isDeviceKind(browse.target.value?.kind)"
      context="scan"
    />
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
