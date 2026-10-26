<script setup lang="ts">
import { computed, ref, shallowRef, useId, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { useHostErrorText } from '../composables/useHostErrorText'
import { listFiles, toHostError, type FsEntry, type HostError } from '../services/host'
import { formatBytes } from '../utils/format'

/** 树里的一项：目录项本身，以及它在分区里的路径（一级一项） */
export interface TreeItem {
  /** 行的唯一标识：上级的标识加上它在排好序的目录里的序号，同名、同定位信息的项也不会混 */
  id: string
  entry: FsEntry
  path: string[]
}

interface DirState {
  entries: FsEntry[]
  total: number
  isLoading: boolean
  error: HostError | null
  overwritten: boolean
  truncated: boolean
}

interface EntryRow {
  kind: 'entry'
  id: string
  entry: FsEntry
  depth: number
  parentId: string
  isExpanded: boolean
}

/** 目录末尾的状态行：正在加载、还有更多（滚到这里自动加载）、加载失败、空目录、已被覆盖 */
interface StatusRow {
  kind: 'status'
  id: string
  dirId: string
  depth: number
  status: 'loading' | 'more' | 'error' | 'empty' | 'overwritten' | 'truncated'
}

type TreeRow = EntryRow | StatusRow

const ROOT_ID = 'root'
const ROW_HEIGHT = 28
const VIEWPORT_ROWS = 16
const OVERSCAN = 6
/** 每次向引擎要这么多项 */
const PAGE_SIZE = 1000
const INDENT_PX = 18

const props = defineProps<{
  jobId: string
  partition: number
  /** 只显示已删除的文件（目录都显示：删除的文件可能在未删除的目录里） */
  deletedOnly: boolean
}>()
const selected = defineModel<TreeItem | null>('selected', { required: true })
/** 勾选要复制的项，键是行的标识 */
const checked = defineModel<Map<string, TreeItem>>('checked', { required: true })
const emit = defineEmits<{ open: [item: TreeItem] }>()

const { t } = useI18n()
const { formatHostError } = useHostErrorText()
const domPrefix = useId()

/** 各目录已加载的内容，键是目录所在行的标识（根目录为 root）；每次更新换一个新 Map */
const dirs = shallowRef(new Map<string, DirState>())
/** 各目录的定位信息与路径，不参与渲染 */
const dirTokens = new Map<string, string | null>()
const dirPaths = new Map<string, string[]>()
const expandedIds = ref(new Set<string>())
const activeId = ref<string | null>(null)
const scrollTop = ref(0)
const viewport = ref<HTMLElement | null>(null)
/** 换分区或重新打开时作废旧请求 */
let generation = 0

const rows = computed<TreeRow[]>(() => {
  const result: TreeRow[] = []
  appendChildren(result, ROOT_ID, 0)
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

watch(
  () => [props.jobId, props.partition] as const,
  () => {
    generation += 1
    dirs.value = new Map()
    dirTokens.clear()
    dirPaths.clear()
    dirTokens.set(ROOT_ID, null)
    dirPaths.set(ROOT_ID, [])
    expandedIds.value = new Set()
    activeId.value = null
    scrollTop.value = 0
    if (viewport.value !== null) {
      viewport.value.scrollTop = 0
    }
    void loadMore(ROOT_ID)
  },
  { immediate: true },
)

// 「还有更多」的行滚进视野就接着加载下一页
watch(visibleRows, (visible) => {
  for (const { row } of visible) {
    if (row.kind === 'status' && row.status === 'more') {
      void loadMore(row.dirId)
    }
  }
})

function appendChildren(result: TreeRow[], dirId: string, depth: number) {
  const state = dirs.value.get(dirId)
  if (state === undefined) {
    return
  }
  let shown = 0
  state.entries.forEach((entry, index) => {
    if (props.deletedOnly && !entry.isDir && !entry.deleted) {
      return
    }
    shown += 1
    const id = `${dirId}/${index}`
    const isExpanded = entry.isDir && expandedIds.value.has(id)
    result.push({ kind: 'entry', id, entry, depth, parentId: dirId, isExpanded })
    if (isExpanded) {
      appendChildren(result, id, depth + 1)
    }
  })
  const status = statusOf(state, shown)
  if (status !== null) {
    result.push({ kind: 'status', id: `${dirId}/#${status}`, dirId, depth, status })
  }
}

function statusOf(state: DirState, shown: number): StatusRow['status'] | null {
  if (state.isLoading) {
    return 'loading'
  }
  if (state.error !== null) {
    return 'error'
  }
  if (state.entries.length < state.total) {
    return 'more'
  }
  if (state.overwritten) {
    return 'overwritten'
  }
  if (state.truncated) {
    return 'truncated'
  }
  return shown === 0 ? 'empty' : null
}

/** 加载目录的下一页；正在加载或已经加载完时什么也不做 */
async function loadMore(dirId: string) {
  const current = dirs.value.get(dirId)
  const isComplete =
    current !== undefined && current.error === null && current.entries.length >= current.total
  if (current?.isLoading === true || isComplete) {
    return
  }
  const token = dirTokens.get(dirId)
  if (token === undefined) {
    return
  }
  const before: DirState = current ?? {
    entries: [],
    total: Number.MAX_SAFE_INTEGER,
    isLoading: false,
    error: null,
    overwritten: false,
    truncated: false,
  }
  updateDir(dirId, { ...before, isLoading: true, error: null })
  const requestGeneration = generation
  try {
    const page = await listFiles(props.jobId, props.partition, token, before.entries.length, PAGE_SIZE)
    if (requestGeneration !== generation) {
      return
    }
    updateDir(dirId, {
      entries: before.entries.concat(page.entries),
      // 引擎一项也没给时按已加载完处理，免得反复请求
      total: page.entries.length === 0 ? before.entries.length : page.total,
      isLoading: false,
      error: null,
      overwritten: page.overwritten,
      truncated: page.truncated,
    })
  } catch (error) {
    if (requestGeneration === generation) {
      updateDir(dirId, { ...before, isLoading: false, error: toHostError(error) })
    }
  }
}

function updateDir(dirId: string, state: DirState) {
  const next = new Map(dirs.value)
  next.set(dirId, state)
  dirs.value = next
}

function toggleDir(row: EntryRow) {
  const next = new Set(expandedIds.value)
  if (next.has(row.id)) {
    next.delete(row.id)
  } else {
    next.add(row.id)
    if (!dirs.value.has(row.id)) {
      dirTokens.set(row.id, row.entry.file)
      dirPaths.set(row.id, itemPath(row))
      void loadMore(row.id)
    }
  }
  expandedIds.value = next
}

function itemPath(row: EntryRow): string[] {
  return [...(dirPaths.get(row.parentId) ?? []), row.entry.name]
}

function toItem(row: EntryRow): TreeItem {
  return { id: row.id, entry: row.entry, path: itemPath(row) }
}

function handleScroll(event: Event) {
  scrollTop.value = (event.target as HTMLElement).scrollTop
}

function handleRowClick(row: TreeRow) {
  activeId.value = row.id
  if (row.kind === 'status') {
    if (row.status === 'error') {
      void loadMore(row.dirId)
    }
    return
  }
  selected.value = toItem(row)
  if (row.entry.isDir) {
    toggleDir(row)
  }
}

function handleRowDoubleClick(row: TreeRow) {
  if (row.kind === 'entry' && !row.entry.isDir) {
    emit('open', toItem(row))
  }
}

function toggleChecked(row: EntryRow) {
  const next = new Map(checked.value)
  if (next.has(row.id)) {
    next.delete(row.id)
  } else {
    next.set(row.id, toItem(row))
  }
  checked.value = next
}

function handleKeydown(event: KeyboardEvent) {
  const current = rows.value[activeIndex.value]
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
      if (current?.kind === 'entry' && current.entry.isDir) {
        if (current.isExpanded) {
          moveTo(activeIndex.value + 1)
        } else {
          toggleDir(current)
        }
      }
      break
    case 'ArrowLeft':
      if (current?.kind === 'entry' && current.isExpanded) {
        toggleDir(current)
      } else if (current !== undefined) {
        const parentId = current.kind === 'entry' ? current.parentId : current.dirId
        moveTo(rows.value.findIndex((row) => row.id === parentId))
      }
      break
    case 'Enter':
      if (current?.kind === 'entry') {
        if (current.entry.isDir) {
          toggleDir(current)
        } else {
          emit('open', toItem(current))
        }
      } else if (current?.kind === 'status' && current.status === 'error') {
        void loadMore(current.dirId)
      }
      break
    case ' ':
      if (current?.kind === 'entry') {
        toggleChecked(current)
      }
      break
    default:
      return
  }
  event.preventDefault()
}

/** 移到第 `index` 行（越界时取首尾），选中它，并滚到看得见 */
function moveTo(index: number) {
  if (rows.value.length === 0 || index < 0) {
    return
  }
  const clamped = Math.min(index, rows.value.length - 1)
  const row = rows.value[clamped]
  if (row === undefined) {
    return
  }
  activeId.value = row.id
  if (row.kind === 'entry') {
    selected.value = toItem(row)
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

function domId(rowId: string): string {
  return `${domPrefix}-${rowId.replace(/[^\w-]/g, '_')}`
}

function statusText(row: StatusRow): string {
  const state = dirs.value.get(row.dirId)
  if (row.status === 'error' && state?.error) {
    return t('undelete.browser.status.error', { reason: formatHostError(state.error) })
  }
  if (row.status === 'more' && state !== undefined) {
    return t('undelete.browser.status.more', { count: state.total - state.entries.length })
  }
  return t(`undelete.browser.status.${row.status}`)
}

function entryBadge(entry: FsEntry): string | null {
  return entry.deleted ? t(`undelete.state.${entry.state}`) : null
}
</script>

<template>
  <div class="fs-browser">
    <div
      ref="viewport"
      class="fs-browser__viewport"
      role="tree"
      tabindex="0"
      :aria-label="t('undelete.browser.title')"
      :aria-activedescendant="activeDomId"
      :style="{ height: `${VIEWPORT_ROWS * ROW_HEIGHT}px` }"
      @scroll="handleScroll"
      @keydown="handleKeydown"
    >
      <div
        class="fs-browser__spacer"
        role="presentation"
        :style="{ height: `${rows.length * ROW_HEIGHT}px` }"
      >
        <div
          v-for="{ row, index } in visibleRows"
          :id="domId(row.id)"
          :key="row.id"
          class="fs-browser__row"
          :class="{
            'fs-browser__row--active': row.id === activeId,
            'fs-browser__row--selected': row.kind === 'entry' && row.id === selected?.id,
            'fs-browser__row--deleted': row.kind === 'entry' && row.entry.deleted,
            'fs-browser__row--status': row.kind === 'status',
          }"
          :style="{
            transform: `translateY(${index * ROW_HEIGHT}px)`,
            height: `${ROW_HEIGHT}px`,
            paddingLeft: `${row.depth * INDENT_PX + 8}px`,
          }"
          role="treeitem"
          :aria-level="row.depth + 1"
          :aria-expanded="row.kind === 'entry' && row.entry.isDir ? row.isExpanded : undefined"
          :aria-selected="row.kind === 'entry' ? row.id === selected?.id : undefined"
          @click="handleRowClick(row)"
          @dblclick="handleRowDoubleClick(row)"
        >
          <template v-if="row.kind === 'entry'">
            <input
              class="fs-browser__check"
              type="checkbox"
              tabindex="-1"
              :checked="checked.has(row.id)"
              :aria-label="t('undelete.browser.check', { name: row.entry.name })"
              @click.stop="toggleChecked(row)"
            />
            <span class="fs-browser__chevron" aria-hidden="true">
              {{ row.entry.isDir ? (row.isExpanded ? '▾' : '▸') : '' }}
            </span>
            <span
              class="fs-browser__name"
              :class="{ 'fs-browser__name--dir': row.entry.isDir }"
              :title="row.entry.nameUncertain ? t('undelete.browser.nameUncertain') : undefined"
            >
              {{ row.entry.name }}
            </span>
            <span
              v-if="entryBadge(row.entry)"
              class="fs-browser__badge"
              :class="`fs-browser__badge--${row.entry.state}`"
            >
              {{ entryBadge(row.entry) }}
            </span>
            <span class="fs-browser__size">
              {{ row.entry.isDir ? '' : formatBytes(row.entry.size) }}
            </span>
            <span class="fs-browser__time">{{ row.entry.modified ?? '' }}</span>
          </template>
          <span v-else class="fs-browser__status">{{ statusText(row) }}</span>
        </div>
      </div>
    </div>
    <p class="fs-browser__hint">{{ t('undelete.browser.hint') }}</p>
  </div>
</template>

<style scoped>
.fs-browser {
  display: flex;
  flex-direction: column;
  gap: var(--space-sm);
  width: 100%;
  min-width: 0;
}

.fs-browser__viewport {
  position: relative;
  overflow-y: auto;
  border: 1px solid var(--color-border);
  border-radius: var(--radius-card);
}

.fs-browser__viewport:focus-visible {
  outline: 2px solid var(--color-accent);
  outline-offset: 1px;
}

.fs-browser__spacer {
  position: relative;
}

.fs-browser__row {
  position: absolute;
  top: 0;
  left: 0;
  right: 0;
  display: flex;
  align-items: center;
  gap: var(--space-xs);
  padding-right: var(--space-sm);
  cursor: default;
  user-select: none;
  font-variant-numeric: tabular-nums;
}

.fs-browser__row--selected {
  background: color-mix(in srgb, var(--color-accent) 16%, transparent);
}

/* 键盘焦点所在的行；只在列表有焦点时画出来 */
.fs-browser__viewport:focus-visible .fs-browser__row--active {
  box-shadow: inset 0 0 0 1px var(--color-accent);
}

.fs-browser__check {
  flex: none;
  margin: 0;
  accent-color: var(--color-accent);
}

.fs-browser__chevron {
  flex: none;
  width: var(--space-md);
  color: var(--color-text-muted);
  text-align: center;
}

.fs-browser__name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.fs-browser__name--dir {
  font-weight: 600;
}

/* 已删除：名字划线、变淡，旁边另有文字标签，不只靠颜色区分 */
.fs-browser__row--deleted .fs-browser__name {
  color: var(--color-text-muted);
  text-decoration: line-through;
}

.fs-browser__badge {
  flex: none;
  padding: 0 var(--space-xs);
  font-size: var(--font-size-small);
  border: 1px solid var(--color-border);
  border-radius: var(--space-xs);
}

.fs-browser__badge--intact {
  color: var(--color-accent);
  border-color: var(--color-accent);
}

.fs-browser__badge--overwritten {
  color: var(--color-danger);
  border-color: var(--color-danger);
}

.fs-browser__size,
.fs-browser__time {
  flex: none;
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
  text-align: right;
}

.fs-browser__size {
  min-width: 72px;
}

.fs-browser__time {
  min-width: 132px;
}

.fs-browser__row--status {
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
}

.fs-browser__status {
  padding-left: calc(var(--space-md) + var(--space-md));
}

.fs-browser__hint {
  margin: 0;
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
}
</style>
