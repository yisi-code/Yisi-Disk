<script lang="ts">
export interface MenuItem {
  key: string
  label: string
  disabled?: boolean
}
</script>

<script setup lang="ts">
import { nextTick, onBeforeUnmount, onMounted, ref } from 'vue'

/** 右键菜单：在 (x, y) 处弹出，靠近窗口边缘时往回挪。打开时焦点落在第一个可用项上，关闭后由父组件把焦点还回去 */
const props = defineProps<{
  items: MenuItem[]
  x: number
  y: number
  label: string
}>()
const emit = defineEmits<{ select: [key: string]; close: [] }>()

/** 菜单离窗口边缘至少留这么多像素 */
const EDGE_GAP = 4

const menuElement = ref<HTMLElement | null>(null)
const itemElements = ref<HTMLButtonElement[]>([])
const position = ref({ left: props.x, top: props.y })

onMounted(async () => {
  await nextTick()
  const menu = menuElement.value
  if (menu !== null) {
    const { width, height } = menu.getBoundingClientRect()
    position.value = {
      left: Math.max(EDGE_GAP, Math.min(props.x, window.innerWidth - width - EDGE_GAP)),
      top: Math.max(EDGE_GAP, Math.min(props.y, window.innerHeight - height - EDGE_GAP)),
    }
  }
  focusItem(enabledIndexes()[0] ?? -1)
  // 下一次按下（包括这次右键之后的）落在菜单外就关闭；捕获阶段注册，别处阻止冒泡也照样收到
  document.addEventListener('pointerdown', handleOutsidePointer, true)
  window.addEventListener('blur', close)
  window.addEventListener('resize', close)
})

onBeforeUnmount(() => {
  document.removeEventListener('pointerdown', handleOutsidePointer, true)
  window.removeEventListener('blur', close)
  window.removeEventListener('resize', close)
})

function handleOutsidePointer(event: PointerEvent) {
  if (menuElement.value !== null && !menuElement.value.contains(event.target as Node)) {
    close()
  }
}

function handleKeydown(event: KeyboardEvent) {
  const enabled = enabledIndexes()
  const current = enabled.indexOf(itemElements.value.indexOf(document.activeElement as HTMLButtonElement))
  switch (event.key) {
    case 'ArrowDown':
      focusItem(enabled[(current + 1) % enabled.length] ?? -1)
      break
    case 'ArrowUp':
      focusItem(enabled[(current - 1 + enabled.length) % enabled.length] ?? -1)
      break
    case 'Home':
      focusItem(enabled[0] ?? -1)
      break
    case 'End':
      focusItem(enabled.at(-1) ?? -1)
      break
    case 'Escape':
    case 'Tab':
      close()
      break
    default:
      return
  }
  event.preventDefault()
}

function handleSelect(item: MenuItem) {
  if (!item.disabled) {
    emit('select', item.key)
  }
}

function close() {
  emit('close')
}

function enabledIndexes(): number[] {
  return props.items.flatMap((item, index) => (item.disabled ? [] : [index]))
}

function focusItem(index: number) {
  itemElements.value[index]?.focus()
}
</script>

<template>
  <Teleport to="body">
    <div
      ref="menuElement"
      class="context-menu"
      role="menu"
      :aria-label="label"
      :style="{ left: `${position.left}px`, top: `${position.top}px` }"
      @keydown="handleKeydown"
      @contextmenu.prevent
    >
      <button
        v-for="item in items"
        :key="item.key"
        ref="itemElements"
        class="context-menu__item"
        type="button"
        role="menuitem"
        tabindex="-1"
        :aria-disabled="item.disabled ? 'true' : undefined"
        @click="handleSelect(item)"
      >
        {{ item.label }}
      </button>
    </div>
  </Teleport>
</template>

<style scoped>
.context-menu {
  position: fixed;
  z-index: 100;
  display: flex;
  flex-direction: column;
  min-width: 180px;
  padding: var(--space-xs) 0;
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--space-xs);
  box-shadow: 0 4px 16px rgb(0 0 0 / 18%);
}

.context-menu__item {
  padding: var(--space-xs) var(--space-md);
  color: var(--color-text);
  background: none;
  border: none;
  font: inherit;
  text-align: left;
  cursor: pointer;
}

.context-menu__item:hover:not([aria-disabled='true']),
.context-menu__item:focus-visible {
  background: color-mix(in srgb, var(--color-accent) 16%, transparent);
  outline: none;
}

.context-menu__item[aria-disabled='true'] {
  color: var(--color-text-muted);
  cursor: default;
}
</style>
