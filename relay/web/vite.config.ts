import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// base './' → 资源用相对路径，适配 rust-embed 由后端 :8000 托管。
// 开发：npm run dev(5173)，/api 代理到后端 :8000；WHEP(:8900) 用绝对地址直连。
export default defineConfig({
  base: './',
  plugins: [react()],
  server: {
    port: 5173,
    proxy: { '/api': 'http://localhost:8000' },
  },
  build: { outDir: 'dist', emptyOutDir: true },
})
