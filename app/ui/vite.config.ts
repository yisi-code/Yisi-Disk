import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

// 开发服务器只供 Tauri 开发窗口使用：固定端口，只绑本机回环地址
export default defineConfig({
  plugins: [vue()],
  clearScreen: false,
  server: {
    host: '127.0.0.1',
    port: 1420,
    strictPort: true,
  },
  define: {
    __VUE_I18N_FULL_INSTALL__: true,
    __VUE_I18N_LEGACY_API__: false,
    __INTLIFY_PROD_DEVTOOLS__: false,
  },
})
