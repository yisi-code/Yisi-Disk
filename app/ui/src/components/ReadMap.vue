<script setup lang="ts">
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import type { ReadMapCell } from '../services/host'
import { formatBytes } from '../utils/format'

/** 每行最多这么多格 */
const MAX_COLUMNS = 64

/**
 * 扇区图：把整块设备平分成格子，每格按读的情况上色。颜色之外，每格悬停有位置与状态的文字，
 * 图例写明每种各有几格；整张图对读屏软件只读一句汇总。`states` 是图例列出的状态（按用途不同）。
 */
const props = withDefaults(
  defineProps<{
    cells: ReadMapCell[]
    cellBytes: number
    size: number
    states?: ReadMapCell[]
  }>(),
  { states: () => ['good', 'slow', 'partly', 'unreadable'] },
)

const { t } = useI18n()

const columns = computed(() => Math.min(MAX_COLUMNS, Math.max(1, props.cells.length)))
const counts = computed(() =>
  props.states.map((state) => ({
    state,
    count: props.cells.filter((cell) => cell === state).length,
  })),
)
const summary = computed(() =>
  t('readMap.summary', {
    cells: props.cells.length,
    size: formatBytes(props.cellBytes),
    counts: counts.value.map(legendText).join(t('readMap.separator')),
  }),
)

function legendText(item: { state: ReadMapCell; count: number }): string {
  return t('readMap.legend', { state: t(`readMap.state.${item.state}`), count: item.count })
}

function cellTitle(index: number, cell: ReadMapCell): string {
  const start = index * props.cellBytes
  const end = Math.min(props.size, start + props.cellBytes)
  return t('readMap.cell', {
    start: formatBytes(start),
    end: formatBytes(end),
    state: t(`readMap.state.${cell}`),
  })
}
</script>

<template>
  <figure class="read-map">
    <figcaption class="read-map__caption">{{ t('readMap.title', { size: formatBytes(cellBytes) }) }}</figcaption>
    <div class="read-map__grid" role="img" :aria-label="summary" :style="{ '--read-map-columns': columns }">
      <span
        v-for="(cell, index) in cells"
        :key="index"
        :class="['read-map__cell', `read-map__cell--${cell}`]"
        :title="cellTitle(index, cell)"
      ></span>
    </div>
    <ul class="read-map__legend">
      <li v-for="item in counts" :key="item.state" class="read-map__legend-item">
        <span :class="['read-map__cell', `read-map__cell--${item.state}`]" aria-hidden="true"></span>
        {{ legendText(item) }}
      </li>
    </ul>
  </figure>
</template>

<style scoped>
.read-map {
  display: flex;
  flex-direction: column;
  gap: var(--space-xs);
  width: 100%;
  margin: 0;
}

.read-map__caption {
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
}

.read-map__grid {
  display: grid;
  grid-template-columns: repeat(var(--read-map-columns), minmax(0, 1fr));
  gap: 1px;
  max-width: 640px;
  padding: 1px;
  background: var(--color-border);
  border-radius: var(--radius-control);
}

.read-map__cell {
  display: inline-block;
  aspect-ratio: 1;
  min-width: 6px;
}

.read-map__cell--good {
  background: color-mix(in srgb, var(--color-success) 55%, var(--color-surface));
}

.read-map__cell--slow {
  background: var(--color-warning);
}

.read-map__cell--partly {
  background: color-mix(in srgb, var(--color-danger) 55%, var(--color-surface));
}

.read-map__cell--unreadable {
  background: var(--color-danger);
}

/* 与底色相同，图例里单独的色块靠描边才看得见 */
.read-map__cell--untried {
  background: var(--color-surface);
  box-shadow: inset 0 0 0 1px var(--color-border);
}

.read-map__cell--pending {
  background: color-mix(in srgb, var(--color-success) 25%, var(--color-surface));
}

.read-map__legend {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-xs) var(--space-md);
  margin: 0;
  padding: 0;
  list-style: none;
  font-size: var(--font-size-small);
}

.read-map__legend-item {
  display: flex;
  align-items: center;
  gap: var(--space-xs);
}

.read-map__legend-item .read-map__cell {
  width: 10px;
  min-width: 10px;
}
</style>
