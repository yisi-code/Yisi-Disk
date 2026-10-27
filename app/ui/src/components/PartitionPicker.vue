<script setup lang="ts">
import { useId } from 'vue'
import { useI18n } from 'vue-i18n'
import { usePartitionText } from '../composables/usePartitionText'
import type { PartitionInfo } from '../services/host'
import { formatBytes } from '../utils/format'

/**
 * 选择分区的表格：编号、卷标、文件系统、用途、起始位置、大小，单选。不能选的行变淡，
 * 文件系统后面跟一句附注（不能选的原因等）。找回删除的文件与检查文件系统共用
 */
const props = defineProps<{
  partitions: PartitionInfo[]
  caption: string
  /** 这个分区能不能选 */
  isSelectable: (partition: PartitionInfo) => boolean
  /** 文件系统后面的附注；空串时不显示 */
  note: (partition: PartitionInfo) => string
  /** 整个表格暂时不能改选（例如正在扫描） */
  disabled?: boolean
}>()
const selected = defineModel<number | null>({ required: true })

const { t } = useI18n()
const { filesystemName, roleName, partitionLabel } = usePartitionText()
const domPrefix = useId()
</script>

<template>
  <table class="wizard__table partition-picker">
    <caption class="wizard__caption">{{ caption }}</caption>
    <thead>
      <tr>
        <th scope="col"><span class="wizard__visually-hidden">{{ t('partition.picker.choose') }}</span></th>
        <th scope="col">{{ t('partition.picker.number') }}</th>
        <th scope="col">{{ t('partition.picker.label') }}</th>
        <th scope="col">{{ t('partition.picker.filesystem') }}</th>
        <th scope="col">{{ t('partition.picker.role') }}</th>
        <th scope="col" class="wizard__number">{{ t('partition.picker.start') }}</th>
        <th scope="col" class="wizard__number">{{ t('partition.picker.size') }}</th>
      </tr>
    </thead>
    <tbody>
      <tr
        v-for="partition in partitions"
        :key="partition.number"
        :class="{ 'partition-picker__row--disabled': !props.isSelectable(partition) }"
      >
        <td>
          <input
            :id="`${domPrefix}-${partition.number}`"
            v-model="selected"
            type="radio"
            :name="domPrefix"
            :value="partition.number"
            :disabled="!props.isSelectable(partition) || disabled"
          />
        </td>
        <th scope="row">
          <label :for="`${domPrefix}-${partition.number}`">
            {{ partition.number }}{{ partition.logical ? ` ${t('partition.picker.logical')}` : '' }}
          </label>
        </th>
        <td>{{ partitionLabel(partition) }}</td>
        <td>
          {{ filesystemName(partition.filesystem) }}
          <span v-if="props.note(partition)" class="wizard__note">· {{ props.note(partition) }}</span>
        </td>
        <td>{{ roleName(partition) }}</td>
        <td class="wizard__number">{{ formatBytes(partition.offset) }}</td>
        <td class="wizard__number">{{ formatBytes(partition.length) }}</td>
      </tr>
    </tbody>
  </table>
</template>

<style scoped>
.partition-picker {
  max-width: none;
}

.partition-picker__row--disabled {
  color: var(--color-text-muted);
}
</style>
