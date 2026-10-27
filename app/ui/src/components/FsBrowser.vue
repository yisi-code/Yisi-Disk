<script setup lang="ts">
import { computed, ref, shallowRef, useId, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { useHostErrorText } from '../composables/useHostErrorText'
import { useTreeNavigation } from '../composables/useTreeNavigation'
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
/** 列表框至少这么多行高；更高时由所在布局拉伸，跟着窗口变 */
const MIN_ROWS = 10
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
/** 换分区或重新打开时作废旧请求 */
let generation = 0

const rows = computed<TreeRow[]>(() => {
  const result: TreeRow[] = []
  appendChildren(result, ROOT_ID, 0)
  return result
})

const {
  activeId,
  activeIndex,
  visibleRows,
  activeDomId,
  domId,
  handleScroll,
  resetScroll,
  moveTo,
  handleNavigationKey,
} = useTreeNavigation(rows, {
  viewportRef: 'viewport',
  rowHeight: ROW_HEIGHT,
  minRows: MIN_ROWS,
  overscan: OVERSCAN,
  domPrefix,
  onMove: (row) => {
    if (row.kind === 'entry') {
      selected.value = toItem(row)
    }
  },
})

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
    resetScroll()
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
      entries: before.entries.concat(page.entries.map(withShownName)),
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

/** 上级文件夹已勾选：它会连同文件夹一起复制，勾选框显示为已勾选、不能单独取消 */
function isCoveredByParent(id: string): boolean {
  for (let end = id.lastIndexOf('/'); end > 0; end = id.lastIndexOf('/', end - 1)) {
    if (checked.value.has(id.slice(0, end))) {
      return true
    }
  }
  return false
}

/** 上级已勾选的项点了不变：拦下浏览器自己的切换，勾选框保持已勾选 */
function handleCheckClick(event: MouseEvent, row: EntryRow) {
  if (isCoveredByParent(row.id)) {
    event.preventDefault()
    return
  }
  toggleChecked(row)
}

function toggleChecked(row: EntryRow) {
  if (isCoveredByParent(row.id)) {
    return
  }
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
  if (handleNavigationKey(event.key)) {
    event.preventDefault()
    return
  }
  switch (event.key) {
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

function statusText(row: StatusRow): string {
  const state = dirs.value.get(row.dirId)
  if (row.status === 'error' && state?.error) {
    return t('partitionFiles.browser.status.error', { reason: formatHostError(state.error) })
  }
  if (row.status === 'more' && state !== undefined) {
    return t('partitionFiles.browser.status.more', { count: state.total - state.entries.length })
  }
  return t(`partitionFiles.browser.status.${row.status}`)
}

function entryBadge(entry: FsEntry): string | null {
  return entry.deleted ? t(`partitionFiles.state.${entry.state}`) : null
}

/** 虚拟目录的名字由界面给出；复制时它也是输出目录里的文件夹名 */
function withShownName(entry: FsEntry): FsEntry {
  return entry.virtualDir === 'orphans' ? { ...entry, name: t('partitionFiles.browser.orphans') } : entry
}
</script>

<template>
  <div class="tree-list">
    <div class="tree-list__toolbar">
      <h3 :id="`${domPrefix}-title`" class="tree-list__title">
        {{ t('partitionFiles.browser.title') }}
      </h3>
      <slot name="tools"></slot>
    </div>
    <div
      ref="viewport"
      class="tree-list__viewport"
      role="tree"
      tabindex="0"
      :aria-labelledby="`${domPrefix}-title`"
      :aria-activedescendant="activeDomId"
      :style="{ minHeight: `${MIN_ROWS * ROW_HEIGHT}px` }"
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
              class="tree-list__check"
              type="checkbox"
              tabindex="-1"
              :checked="checked.has(row.id) || isCoveredByParent(row.id)"
              :aria-disabled="isCoveredByParent(row.id) || undefined"
              :title="isCoveredByParent(row.id) ? t('partitionFiles.browser.checkInherited') : undefined"
              :aria-label="t('partitionFiles.browser.check', { name: row.entry.name })"
              @click.stop="handleCheckClick($event, row)"
            />
            <span class="fs-browser__chevron" aria-hidden="true">
              {{ row.entry.isDir ? (row.isExpanded ? '▾' : '▸') : '' }}
            </span>
            <span
              class="fs-browser__name"
              :class="{
                'fs-browser__name--dir': row.entry.isDir,
                'fs-browser__name--virtual': row.entry.virtualDir !== null,
              }"
              :title="row.entry.nameUncertain ? t('partitionFiles.browser.nameUncertain') : undefined"
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
            <span
              v-if="row.entry.unsupported"
              class="fs-browser__badge fs-browser__badge--unsupported"
            >
              {{ t(`partitionFiles.unsupported.${row.entry.unsupported}`) }}
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
    <div class="fs-browser__footer">
      <p class="tree-list__hint">{{ t('partitionFiles.browser.hint') }}</p>
      <slot name="actions"></slot>
    </div>
  </div>
</template>

<style scoped>

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
.tree-list__viewport:focus-visible .fs-browser__row--active {
  box-shadow: inset 0 0 0 1px var(--color-accent);
}

/* 随上级文件夹一起复制：仍是已勾选的样子，只是淡一些、点了不变 */
.tree-list__check[aria-disabled='true'] {
  opacity: 0.6;
  cursor: not-allowed;
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

/* 虚拟目录（找不到原目录的文件）不在卷上 */
.fs-browser__name--virtual {
  font-style: italic;
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
  border-radius: var(--radius-control);
}

.fs-browser__badge--intact {
  color: var(--color-accent);
  border-color: var(--color-accent);
}

.fs-browser__badge--overwritten {
  color: var(--color-danger);
  border-color: var(--color-danger);
}

.fs-browser__badge--unsupported {
  color: var(--color-text-muted);
  border-style: dashed;
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

.fs-browser__footer {
  display: flex;
  align-items: flex-start;
  gap: var(--space-md);
}

</style>
