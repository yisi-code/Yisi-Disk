<script setup lang="ts">
import { useI18n } from 'vue-i18n'
import type { ByteRange } from '../services/host'
import { formatBytes } from '../utils/format'

/**
 * 读不出的区段表：起始位置（换算的大小与字节偏移）与长度。坏道检测与制作镜像共用；
 * 只列出前多少段、记不下全部时怎么说明，由使用方在表格下面写
 */
defineProps<{
  ranges: ByteRange[]
}>()

const { t, locale } = useI18n()

function formatPosition(offset: number): string {
  return t('common.ranges.positionValue', {
    position: formatBytes(offset),
    offset: offset.toLocaleString(locale.value),
  })
}
</script>

<template>
  <table class="wizard__table">
    <caption class="wizard__caption">{{ t('common.ranges.caption') }}</caption>
    <thead>
      <tr>
        <th scope="col">{{ t('common.ranges.start') }}</th>
        <th scope="col" class="wizard__number">{{ t('common.ranges.length') }}</th>
      </tr>
    </thead>
    <tbody>
      <tr v-for="range in ranges" :key="range.offset">
        <td>{{ formatPosition(range.offset) }}</td>
        <td class="wizard__number">{{ formatBytes(range.length) }}</td>
      </tr>
    </tbody>
  </table>
</template>
