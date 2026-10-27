<script setup lang="ts">
import { computed, ref, useId, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { useFileTypeName } from '../composables/useFileTypeName'
import type { FileType, FoundFile } from '../services/host'
import { formatBytes } from '../utils/format'

type GroupMode = 'type' | 'position'

interface FileGroup {
  /** 带分组方式前缀，两种分组的展开状态互不干扰 */
  key: string
  label: string
  files: FoundFile[]
  bytes: number
}

interface GroupRow {
  kind: 'group'
  id: string
  group: FileGroup
  isExpanded: boolean
  position: number
  setSize: number
}

interface FileRow {
  kind: 'file'
  id: string
  file: FoundFile
  groupId: string
  position: number
  setSize: number
}

type TreeRow = GroupRow | FileRow

/** 行高固定，按滚动位置只渲染看得见的行 */
const ROW_HEIGHT = 28
const VIEWPORT_ROWS = 14
const OVERSCAN = 6
/** 按磁盘位置分组时最多分这么多段，每段至少 1 MiB、按 2 的幂取整 */
const MAX_POSITION_GROUPS = 32
const MIN_POSITION_SPAN = 1024 * 1024

const props = defineProps<{
  files: FoundFile[]
  fileTypes: FileType[]
  /** 源的总大小，决定按位置分组时每段多大 */
  deviceSize: number
  /** 「只扫描」时可以勾选要保存的文件 */
  selectable: boolean
}>()
const selectedFile = defineModel<FoundFile | null>('selected', { required: true })
/** 勾选要保存的文件（按文件名）；每次改动换一个新 Set */
const checkedNames = defineModel<Set<string>>('checked', { default: () => new Set<string>() })
const emit = defineEmits<{
  open: [file: FoundFile]
  /** 右键或菜单键：在 (x, y) 处弹出这个文件的操作菜单 */
  menu: [file: FoundFile, point: { x: number; y: number }]
}>()

const { t } = useI18n()
const { fileTypeName } = useFileTypeName()
const domPrefix = useId()

const groupMode = ref<GroupMode>('type')
const expandedKeys = ref(new Set<string>())
const activeId = ref<string | null>(null)
const scrollTop = ref(0)
const viewport = ref<HTMLElement | null>(null)

const groups = computed<FileGroup[]>(() =>
  groupMode.value === 'type' ? groupByType() : groupByPosition(),
)

const rows = computed<TreeRow[]>(() => {
  const result: TreeRow[] = []
  groups.value.forEach((group, groupIndex) => {
    const groupId = `g-${group.key}`
    const isExpanded = expandedKeys.value.has(group.key)
    result.push({
      kind: 'group',
      id: groupId,
      group,
      isExpanded,
      position: groupIndex + 1,
      setSize: groups.value.length,
    })
    if (isExpanded) {
      group.files.forEach((file, fileIndex) => {
        result.push({
          kind: 'file',
          id: `f-${file.name}`,
          file,
          groupId,
          position: fileIndex + 1,
          setSize: group.files.length,
        })
      })
    }
  })
  return result
})

const activeIndex = computed(() => rows.value.findIndex((row) => row.id === activeId.value))

const visibleRows = computed(() => {
  const first = Math.max(0, Math.floor(scrollTop.value / ROW_HEIGHT) - OVERSCAN)
  const last = Math.min(rows.value.length, first + VIEWPORT_ROWS + OVERSCAN * 2)
  return rows.value.slice(first, last).map((row, offset) => ({ row, index: first + offset }))
})

const activeDomId = computed(() =>
  activeIndex.value >= 0 && activeId.value !== null ? domId(activeId.value) : undefined,
)

// 新的一次恢复从空列表开始：清掉上一次的展开与焦点
watch(
  () => props.files.length === 0,
  (isEmpty) => {
    if (isEmpty) {
      expandedKeys.value = new Set()
      activeId.value = null
      scrollTop.value = 0
    }
  },
)

watch(groupMode, () => {
  activeId.value = null
  scrollTop.value = 0
  if (viewport.value !== null) {
    viewport.value.scrollTop = 0
  }
})

function handleScroll(event: Event) {
  scrollTop.value = (event.target as HTMLElement).scrollTop
}

function handleRowClick(row: TreeRow) {
  activeId.value = row.id
  if (row.kind === 'group') {
    toggleGroup(row.group.key)
  } else {
    selectedFile.value = row.file
  }
}

function handleRowDoubleClick(row: TreeRow) {
  if (row.kind === 'file') {
    emit('open', row.file)
  }
}

/** 右键文件行：先选中它，再在鼠标处弹出菜单 */
function handleRowMenu(row: TreeRow, event: MouseEvent) {
  if (row.kind !== 'file') {
    return
  }
  activeId.value = row.id
  selectedFile.value = row.file
  emit('menu', row.file, { x: event.clientX, y: event.clientY })
}

/** 菜单键或 Shift+F10：在当前行下方弹出菜单 */
function openMenuFromKeyboard(row: TreeRow | undefined) {
  if (row?.kind !== 'file') {
    return
  }
  const rect = document.getElementById(domId(row.id))?.getBoundingClientRect()
  emit('menu', row.file, {
    x: rect === undefined ? 0 : rect.left + ROW_HEIGHT,
    y: rect === undefined ? 0 : rect.bottom,
  })
}

/** 菜单关闭后，父组件把焦点还给列表 */
function focus() {
  viewport.value?.focus()
}

defineExpose({ focus })

function handleKeydown(event: KeyboardEvent) {
  const current = rows.value[activeIndex.value]
  if (event.key === 'ContextMenu' || (event.key === 'F10' && event.shiftKey)) {
    openMenuFromKeyboard(current)
    event.preventDefault()
    return
  }
  switch (event.key) {
    case 'ArrowDown':
      moveTo(activeIndex.value + 1)
      break
    case 'ArrowUp':
      moveTo(activeIndex.value < 0 ? 0 : activeIndex.value - 1)
      break
    case 'PageDown':
      moveTo(activeIndex.value + VIEWPORT_ROWS)
      break
    case 'PageUp':
      moveTo(activeIndex.value - VIEWPORT_ROWS)
      break
    case 'Home':
      moveTo(0)
      break
    case 'End':
      moveTo(rows.value.length - 1)
      break
    case 'ArrowRight':
      if (current?.kind === 'group') {
        if (current.isExpanded) {
          moveTo(activeIndex.value + 1)
        } else {
          toggleGroup(current.group.key)
        }
      }
      break
    case 'ArrowLeft':
      if (current?.kind === 'group' && current.isExpanded) {
        toggleGroup(current.group.key)
      } else if (current?.kind === 'file') {
        moveTo(rows.value.findIndex((row) => row.id === current.groupId))
      }
      break
    case 'Enter':
      if (current?.kind === 'group') {
        toggleGroup(current.group.key)
      } else if (current?.kind === 'file') {
        emit('open', current.file)
      }
      break
    case ' ':
      if (props.selectable && current !== undefined) {
        toggleChecked(current)
      } else if (current?.kind === 'group') {
        toggleGroup(current.group.key)
      }
      break
    default:
      return
  }
  event.preventDefault()
}

/** 移到第 `index` 行（越界时取首尾），文件行同时选中，并滚到看得见 */
function moveTo(index: number) {
  if (rows.value.length === 0) {
    return
  }
  const clamped = Math.min(Math.max(index, 0), rows.value.length - 1)
  const row = rows.value[clamped]
  if (row === undefined) {
    return
  }
  activeId.value = row.id
  if (row.kind === 'file') {
    selectedFile.value = row.file
  }
  scrollIntoView(clamped)
}

function scrollIntoView(index: number) {
  const element = viewport.value
  if (element === null) {
    return
  }
  const top = index * ROW_HEIGHT
  if (top < element.scrollTop) {
    element.scrollTop = top
  } else if (top + ROW_HEIGHT > element.scrollTop + element.clientHeight) {
    element.scrollTop = top + ROW_HEIGHT - element.clientHeight
  }
}

/** 勾选或取消一个文件；分组行整组勾选，组里已全部勾选时整组取消 */
function toggleChecked(row: TreeRow) {
  const names = row.kind === 'group' ? row.group.files.map((file) => file.name) : [row.file.name]
  const next = new Set(checkedNames.value)
  const shouldCheck = !names.every((name) => next.has(name))
  for (const name of names) {
    if (shouldCheck) {
      next.add(name)
    } else {
      next.delete(name)
    }
  }
  checkedNames.value = next
}

function isGroupChecked(group: FileGroup): boolean {
  return group.files.length > 0 && group.files.every((file) => checkedNames.value.has(file.name))
}

function toggleGroup(key: string) {
  const next = new Set(expandedKeys.value)
  if (next.has(key)) {
    next.delete(key)
  } else {
    next.add(key)
  }
  expandedKeys.value = next
}

function groupByType(): FileGroup[] {
  const byExtension = new Map<string, FoundFile[]>()
  for (const file of props.files) {
    const bucket = byExtension.get(file.extension)
    if (bucket === undefined) {
      byExtension.set(file.extension, [file])
    } else {
      bucket.push(file)
    }
  }
  // 按格式列表的顺序排列；格式列表里没有的扩展名排在最后
  const order = props.fileTypes.map((type) => type.extension)
  const extensions = [...byExtension.keys()].sort(
    (a, b) => rank(order, a) - rank(order, b) || a.localeCompare(b),
  )
  return extensions.map((extension) => {
    const files = byExtension.get(extension) ?? []
    const description = props.fileTypes.find((type) => type.extension === extension)?.description
    return {
      key: `type:${extension}`,
      label: fileTypeName(extension, description),
      files,
      bytes: sumBytes(files),
    }
  })
}

function groupByPosition(): FileGroup[] {
  const span = positionSpan()
  const byIndex = new Map<number, FoundFile[]>()
  for (const file of props.files) {
    const index = Math.floor(file.offset / span)
    const bucket = byIndex.get(index)
    if (bucket === undefined) {
      byIndex.set(index, [file])
    } else {
      bucket.push(file)
    }
  }
  return [...byIndex.keys()]
    .sort((a, b) => a - b)
    .map((index) => {
      const files = byIndex.get(index) ?? []
      return {
        key: `position:${span}:${index}`,
        label: t('carve.browser.range', {
          start: formatBytes(index * span),
          end: formatBytes((index + 1) * span),
        }),
        files,
        bytes: sumBytes(files),
      }
    })
}

/** 每段的大小：段数不超过上限的最小 2 的幂，至少 1 MiB */
function positionSpan(): number {
  const lastFile = props.files.at(-1)
  const size = Math.max(props.deviceSize, lastFile === undefined ? 0 : lastFile.offset + 1)
  let span = MIN_POSITION_SPAN
  while (size / span > MAX_POSITION_GROUPS) {
    span *= 2
  }
  return span
}

function rank(order: string[], extension: string): number {
  const index = order.indexOf(extension)
  return index < 0 ? order.length : index
}

function sumBytes(files: FoundFile[]): number {
  return files.reduce((sum, file) => sum + file.length, 0)
}

function domId(rowId: string): string {
  return `${domPrefix}-${rowId.replace(/[^\w-]/g, '_')}`
}

function fileDetail(file: FoundFile): string {
  if (groupMode.value === 'type') {
    return t('carve.browser.at', { position: formatBytes(file.offset) })
  }
  const description = props.fileTypes.find((type) => type.extension === file.extension)?.description
  return fileTypeName(file.extension, description)
}
</script>

<template>
  <div class="recovered-browser">
    <div class="recovered-browser__toolbar">
      <h3 :id="`${domPrefix}-title`" class="recovered-browser__title">
        {{ t('carve.browser.title') }}
      </h3>
      <fieldset class="recovered-browser__modes">
        <legend class="recovered-browser__modes-legend">{{ t('carve.browser.groupBy') }}</legend>
        <label class="recovered-browser__mode">
          <input v-model="groupMode" type="radio" value="type" />
          {{ t('carve.browser.byType') }}
        </label>
        <label class="recovered-browser__mode">
          <input v-model="groupMode" type="radio" value="position" />
          {{ t('carve.browser.byPosition') }}
        </label>
      </fieldset>
    </div>

    <p v-if="files.length === 0" class="recovered-browser__empty">
      {{ t('carve.browser.empty') }}
    </p>
    <div
      v-else
      ref="viewport"
      class="recovered-browser__viewport"
      role="tree"
      tabindex="0"
      :aria-labelledby="`${domPrefix}-title`"
      :aria-activedescendant="activeDomId"
      :style="{ height: `${VIEWPORT_ROWS * ROW_HEIGHT}px` }"
      @scroll="handleScroll"
      @keydown="handleKeydown"
    >
      <div
        class="recovered-browser__spacer"
        role="presentation"
        :style="{ height: `${rows.length * ROW_HEIGHT}px` }"
      >
        <div
          v-for="{ row, index } in visibleRows"
          :id="domId(row.id)"
          :key="row.id"
          class="recovered-browser__row"
          :class="{
            'recovered-browser__row--group': row.kind === 'group',
            'recovered-browser__row--active': row.id === activeId,
            'recovered-browser__row--selected':
              row.kind === 'file' && row.file.name === selectedFile?.name,
          }"
          :style="{ transform: `translateY(${index * ROW_HEIGHT}px)`, height: `${ROW_HEIGHT}px` }"
          role="treeitem"
          :aria-level="row.kind === 'group' ? 1 : 2"
          :aria-expanded="row.kind === 'group' ? row.isExpanded : undefined"
          :aria-selected="row.kind === 'file' ? row.file.name === selectedFile?.name : undefined"
          :aria-setsize="row.setSize"
          :aria-posinset="row.position"
          @click="handleRowClick(row)"
          @dblclick="handleRowDoubleClick(row)"
          @contextmenu.prevent="handleRowMenu(row, $event)"
        >
          <template v-if="row.kind === 'group'">
            <input
              v-if="selectable"
              class="recovered-browser__check"
              type="checkbox"
              tabindex="-1"
              :checked="isGroupChecked(row.group)"
              :aria-label="t('carve.browser.checkGroup', { name: row.group.label })"
              @click.stop="toggleChecked(row)"
            />
            <span class="recovered-browser__chevron" aria-hidden="true">
              {{ row.isExpanded ? '▾' : '▸' }}
            </span>
            <span class="recovered-browser__name">{{ row.group.label }}</span>
            <span class="recovered-browser__meta">
              {{
                t('carve.browser.groupTotals', {
                  count: row.group.files.length,
                  bytes: formatBytes(row.group.bytes),
                })
              }}
            </span>
          </template>
          <template v-else>
            <input
              v-if="selectable"
              class="recovered-browser__check"
              type="checkbox"
              tabindex="-1"
              :checked="checkedNames.has(row.file.name)"
              :aria-label="t('carve.browser.check', { name: row.file.name })"
              @click.stop="toggleChecked(row)"
            />
            <span class="recovered-browser__name recovered-browser__name--file">
              {{ row.file.name }}
            </span>
            <span class="recovered-browser__meta">{{ fileDetail(row.file) }}</span>
            <span class="recovered-browser__size">{{ formatBytes(row.file.length) }}</span>
          </template>
        </div>
      </div>
    </div>
    <div class="recovered-browser__footer">
      <p class="recovered-browser__hint">
        {{ selectable ? t('carve.browser.selectHint') : t('carve.browser.hint') }}
      </p>
      <slot name="actions"></slot>
    </div>
  </div>
</template>

<style scoped>
.recovered-browser {
  display: flex;
  flex-direction: column;
  gap: var(--space-sm);
  width: 100%;
  min-width: 0;
}

.recovered-browser__toolbar {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-sm);
}

.recovered-browser__title {
  margin: 0;
  font-size: var(--font-size-body);
  font-weight: 600;
}

.recovered-browser__modes {
  display: flex;
  align-items: center;
  gap: var(--space-md);
  margin: 0;
  padding: 0;
  border: none;
}

.recovered-browser__modes-legend {
  float: left;
  margin-right: var(--space-xs);
  padding: 0;
  color: var(--color-text-muted);
}

.recovered-browser__mode {
  display: flex;
  align-items: center;
  gap: var(--space-xs);
  cursor: pointer;
}

.recovered-browser__empty,
.recovered-browser__footer {
  display: flex;
  align-items: flex-start;
  gap: var(--space-md);
}

.recovered-browser__hint {
  flex: 1;
  min-width: 0;
  margin: 0;
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
}

.recovered-browser__viewport {
  position: relative;
  overflow-y: auto;
  border: 1px solid var(--color-border);
  border-radius: var(--radius-card);
}

.recovered-browser__viewport:focus-visible {
  outline: 2px solid var(--color-accent);
  outline-offset: 1px;
}

.recovered-browser__spacer {
  position: relative;
}

.recovered-browser__row {
  position: absolute;
  top: 0;
  left: 0;
  right: 0;
  display: flex;
  align-items: center;
  gap: var(--space-sm);
  padding: 0 var(--space-sm) 0 calc(var(--space-sm) + var(--space-md));
  cursor: default;
  user-select: none;
  font-variant-numeric: tabular-nums;
}

.recovered-browser__row--group {
  padding-left: var(--space-sm);
  font-weight: 600;
  cursor: pointer;
}

.recovered-browser__row--selected {
  background: color-mix(in srgb, var(--color-accent) 16%, transparent);
}

/* 键盘焦点所在的行；只在列表有焦点时画出来 */
.recovered-browser__viewport:focus-visible .recovered-browser__row--active {
  box-shadow: inset 0 0 0 1px var(--color-accent);
}

.recovered-browser__check {
  flex: none;
  margin: 0;
  accent-color: var(--color-accent);
}

.recovered-browser__chevron {
  flex: none;
  width: var(--space-md);
  color: var(--color-text-muted);
}

.recovered-browser__name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.recovered-browser__meta,
.recovered-browser__size {
  flex: none;
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
  font-weight: normal;
}

.recovered-browser__size {
  min-width: 72px;
  text-align: right;
}
</style>
