import react from '@vitejs/plugin-react'
import { defineConfig } from 'vitest/config'

// The coordinator serves the built UI itself; in development Vite serves it
// and forwards the API (including the events WebSocket) to a local coordinator.
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:18080',
        ws: true,
      },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
  test: {
    environment: 'jsdom',
    setupFiles: ['./src/test/setup.ts'],
    restoreMocks: true,
  },
})
