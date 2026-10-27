import { onBeforeUnmount, ref, watch, type Ref } from 'vue'

/**
 * 列表框此刻能显示几行：按框的实际高度算，窗口变大变小时跟着变。
 * 虚拟列表据此决定渲染哪些行、PageUp / PageDown 翻多少行；框还没渲染出来时用 `fallbackRows`
 */
export function useVisibleRows(
  viewport: Ref<HTMLElement | null>,
  rowHeight: number,
  fallbackRows: number,
) {
  const visibleRows = ref(fallbackRows)
  const observer = new ResizeObserver((entries) => {
    for (const entry of entries) {
      visibleRows.value = Math.max(1, Math.ceil(entry.contentRect.height / rowHeight))
    }
  })

  // 列表框可能随「没有文件」的提示出现、消失，换了元素就换着观察
  watch(
    viewport,
    (element, previous) => {
      if (previous) {
        observer.unobserve(previous)
      }
      if (element) {
        observer.observe(element)
      }
    },
    { immediate: true, flush: 'post' },
  )

  onBeforeUnmount(() => observer.disconnect())

  return visibleRows
}
