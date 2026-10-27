<script setup lang="ts">
import { useId } from 'vue'
import { useI18n } from 'vue-i18n'
import { usePartitionText } from '../composables/usePartitionText'
import type { ImageRegion, PartitionInfo } from '../services/host'
import { formatBytes } from '../utils/format'

/**
 * 做镜像时选范围：源上的各个分区与分区以外的空隙，可以多选；每行给出这一段做到哪了（未开始、已复制几成、
 * 已完成、坏区附近待处理），抽查与源不一致时标出来。只有制作镜像页用
 */
defineProps<{
  regions: ImageRegion[]
  partitions: PartitionInfo[]
  disabled: boolean
}>()
/** 选中的各段，按起点认 */
const selected = defineModel<number[]>({ required: true })

const { t } = useI18n()
const { regionLine } = usePartitionText()
const domPrefix = useId()

function statusText(region: ImageRegion): string {
  const { readBytes, badBytes, untriedBytes, trimBytes, scrapeBytes } = region.progress
  if (untriedBytes === region.length) {
    return t('diskImage.regions.untouched')
  }
  const pending = trimBytes + scrapeBytes
  if (untriedBytes + pending === 0) {
    return badBytes === 0
      ? t('diskImage.regions.done')
      : t('diskImage.regions.doneWithBad', { bad: formatBytes(badBytes) })
  }
  const percent = Math.floor(((readBytes + badBytes) / region.length) * 100)
  return pending === 0
    ? t('diskImage.regions.partial', { percent })
    : t('diskImage.regions.partialPending', { percent, pending: formatBytes(pending) })
}

function checkText(region: ImageRegion): string {
  const check = region.check
  if (check === null) {
    return ''
  }
  if (check.mismatched > 0) {
    return t('diskImage.regions.changed', { count: check.mismatched, checked: check.checked }, check.mismatched)
  }
  if (check.unreadable > 0) {
    return t('diskImage.regions.checkUnreadable', { count: check.unreadable }, check.unreadable)
  }
  return t('diskImage.regions.checked', { count: check.checked }, check.checked)
}
</script>

<template>
  <table class="wizard__table image-regions">
    <caption class="wizard__caption">{{ t('diskImage.regions.caption') }}</caption>
    <thead>
      <tr>
        <th scope="col"><span class="wizard__visually-hidden">{{ t('diskImage.regions.choose') }}</span></th>
        <th scope="col">{{ t('diskImage.regions.name') }}</th>
        <th scope="col" class="wizard__number">{{ t('partition.picker.start') }}</th>
        <th scope="col">{{ t('diskImage.regions.status') }}</th>
      </tr>
    </thead>
    <tbody>
      <tr v-for="region in regions" :key="region.offset">
        <td>
          <input
            :id="`${domPrefix}-${region.offset}`"
            v-model="selected"
            type="checkbox"
            :value="region.offset"
            :disabled="disabled"
          />
        </td>
        <th scope="row">
          <label :for="`${domPrefix}-${region.offset}`">{{ regionLine(region, partitions) }}</label>
        </th>
        <td class="wizard__number">{{ formatBytes(region.offset) }}</td>
        <td>
          {{ statusText(region) }}
          <span
            v-if="checkText(region)"
            :class="region.check && region.check.mismatched > 0 ? 'wizard__hint wizard__hint--warning' : 'wizard__note'"
          >
            · {{ checkText(region) }}
          </span>
        </td>
      </tr>
    </tbody>
  </table>
</template>

<style scoped>
.image-regions {
  max-width: none;
}
</style>
