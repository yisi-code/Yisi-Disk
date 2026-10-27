import { computed, ref, useTemplateRef, type Ref } from 'vue'
import { useVisibleRows } from './useVisibleRows'

/**
 * 虚拟树列表的共用部分：只渲染看得见的几行、当前行、滚到看得见、上下与翻页键。
 * 分区文件的目录树（`FsBrowser`）与深度恢复的结果树（`RecoveredBrowser`）共用；
 * 左右键、回车、空格在两边含义不同，由组件自己处理
 */
export function useTreeNavigation<Row extends { id: string }>(
  rows: Ref<Row[]>,
  options: {
    /** 列表框在模板里的 `ref` 名字 */
    viewportRef: string
    rowHeight: number
    /** 列表框至少这么多行高 */
    minRows: number
    /** 看得见的行上下各多渲染几行 */
    overscan: number
    /** 行的 DOM id 前缀（`useId()`） */
    domPrefix: string
    /** 移到某一行时（键盘或代码）要做的事，例如选中它 */
    onMove: (row: Row) => void
  },
) {
  const viewport = useTemplateRef<HTMLElement>(options.viewportRef)
  const scrollTop = ref(0)
  const activeId = ref<string | null>(null)
  const rowsInView = useVisibleRows(viewport, options.rowHeight, options.minRows)

  const activeIndex = computed(() => rows.value.findIndex((row) => row.id === activeId.value))

  const visibleRows = computed(() => {
    const first = Math.max(0, Math.floor(scrollTop.value / options.rowHeight) - options.overscan)
    const last = Math.min(rows.value.length, first + rowsInView.value + options.overscan * 2)
    return rows.value.slice(first, last).map((row, offset) => ({ row, index: first + offset }))
  })

  const activeDomId = computed(() =>
    activeIndex.value >= 0 && activeId.value !== null ? domId(activeId.value) : undefined,
  )

  function domId(rowId: string): string {
    return `${options.domPrefix}-${rowId.replace(/[^\w-]/g, '_')}`
  }

  function handleScroll(event: Event) {
    scrollTop.value = (event.target as HTMLElement).scrollTop
  }

  /** 回到顶上：换了一批内容时用 */
  function resetScroll() {
    scrollTop.value = 0
    if (viewport.value) {
      viewport.value.scrollTop = 0
    }
  }

  /** 移到第 `index` 行（超过末尾时取最后一行；小于 0 时不动，例如没找到上级），并滚到看得见 */
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
    options.onMove(row)
    scrollIntoView(clamped)
  }

  function scrollIntoView(index: number) {
    const element = viewport.value
    if (element === null) {
      return
    }
    const top = index * options.rowHeight
    if (top < element.scrollTop) {
      element.scrollTop = top
    } else if (top + options.rowHeight > element.scrollTop + element.clientHeight) {
      element.scrollTop = top + options.rowHeight - element.clientHeight
    }
  }

  /** 上下、翻页、首尾键；处理了返回 true（调用方随后 `preventDefault`） */
  function handleNavigationKey(key: string): boolean {
    switch (key) {
      case 'ArrowDown':
        moveTo(activeIndex.value + 1)
        return true
      case 'ArrowUp':
        moveTo(activeIndex.value < 0 ? 0 : activeIndex.value - 1)
        return true
      case 'PageDown':
        moveTo(activeIndex.value + rowsInView.value)
        return true
      case 'PageUp':
        // 靠近顶上时停在第一行
        moveTo(Math.max(0, activeIndex.value - rowsInView.value))
        return true
      case 'Home':
        moveTo(0)
        return true
      case 'End':
        moveTo(rows.value.length - 1)
        return true
      default:
        return false
    }
  }

  return {
    viewport,
    scrollTop,
    activeId,
    activeIndex,
    visibleRows,
    activeDomId,
    domId,
    handleScroll,
    resetScroll,
    moveTo,
    handleNavigationKey,
  }
}
