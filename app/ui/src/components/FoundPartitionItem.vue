<script setup lang="ts">
import { useI18n } from 'vue-i18n'
import { usePartitionText } from '../composables/usePartitionText'
import type { FoundPartition } from '../services/host'
import { formatBytes } from '../utils/format'

/**
 * 丢失分区搜索找到的一个分区：文件系统与大小、卷标、起始位置，以及与分区表的关系等文字标签（不只靠颜色区分）。
 * 搜索进行中的列表与结果页的单选列表共用
 */
const props = defineProps<{ candidate: FoundPartition }>()

const { t } = useI18n()
const { filesystemName } = usePartitionText()

/** 说明标签：与分区表的关系，以及靠备份找到、与别的候选重叠、超出末尾 */
function tags(): { kind: string; text: string }[] {
  const candidate = props.candidate
  const result: { kind: string; text: string }[] = [
    {
      kind: candidate.relation,
      text: t(`findPartitions.list.relation.${candidate.relation}`, {
        number: candidate.tablePartition ?? '',
      }),
    },
  ]
  if (candidate.foundBy === 'backup') {
    result.push({ kind: 'backup', text: t('findPartitions.list.backup') })
  }
  if (candidate.overlapsCandidate) {
    result.push({ kind: 'overlapsCandidate', text: t('findPartitions.list.overlapsCandidate') })
  }
  if (candidate.truncated) {
    result.push({ kind: 'truncated', text: t('findPartitions.list.truncated') })
  }
  return result
}
</script>

<template>
  <span class="found-partition">
    <span class="found-partition__name">
      {{
        t('findPartitions.list.name', {
          filesystem: filesystemName(candidate.filesystem),
          size: formatBytes(candidate.length),
        })
      }}
      <span class="found-partition__label">
        {{ candidate.label ?? t('findPartitions.list.noLabel') }}
      </span>
    </span>
    <span class="found-partition__start">
      {{ t('findPartitions.list.start', { offset: formatBytes(candidate.offset) }) }}
    </span>
    <span class="found-partition__tags">
      <span
        v-for="tag in tags()"
        :key="tag.kind"
        class="found-partition__tag"
        :class="`found-partition__tag--${tag.kind}`"
      >
        {{ tag.text }}
      </span>
    </span>
  </span>
</template>

<style scoped>
.found-partition {
  display: flex;
  flex: 1 1 auto;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}

.found-partition__name {
  font-weight: 600;
  overflow-wrap: anywhere;
}

.found-partition__label {
  margin-left: var(--space-xs);
  font-weight: normal;
}

.found-partition__start {
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
}

.found-partition__tags {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-xs);
}

.found-partition__tag {
  padding: 0 var(--space-xs);
  font-size: var(--font-size-small);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-control);
}

.found-partition__tag--lost {
  border-color: var(--color-warning);
}
</style>
