import { defineConfig } from 'vite';

export default defineConfig({
  base: './',
  server: {
    port: 5173,
    strictPort: true,
    proxy: {
      // `agent web` on 7431; the dev page carries the same #t=<token> fragment.
      '/api': { target: 'http://127.0.0.1:7431', changeOrigin: false },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
});
