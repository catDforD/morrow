import tailwindcss from '@tailwindcss/vite'
import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

export default defineConfig({
  base: '/assets/',
  plugins: [react(), tailwindcss()],
  server: {
    host: '127.0.0.1',
    port: 5173,
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:3000',
        ws: true,
      },
    },
  },
  build: {
    outDir: '../assets',
    emptyOutDir: false,
    sourcemap: false,
    cssCodeSplit: false,
    rollupOptions: {
      output: {
        codeSplitting: false,
        entryFileNames: 'app.js',
        chunkFileNames: 'app.js',
        assetFileNames: (assetInfo) =>
          assetInfo.names.some((name) => name.endsWith('.css'))
            ? 'style.css'
            : '[name][extname]',
      },
    },
  },
})
