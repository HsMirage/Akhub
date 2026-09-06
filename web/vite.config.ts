import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// 产物由 Rust 二进制内嵌后从 /admin 提供，因此 base 必须是 /admin/。
export default defineConfig({
  plugins: [react()],
  base: '/admin/',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    // 后台是内嵌资源，不需要 source map 体积。
    sourcemap: false,
  },
  server: {
    port: 5173,
    // 开发模式下把 API 打到本地运行的 Akhub。
    proxy: {
      '/admin/api': 'http://127.0.0.1:8080',
      '/v1': 'http://127.0.0.1:8080',
    },
  },
})
