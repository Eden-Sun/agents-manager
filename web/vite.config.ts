import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

// Dev-time daemon endpoint (SPEC §5: `[server] listen = "127.0.0.1:7788"`).
const DAEMON = process.env.VITE_DAEMON ?? 'http://127.0.0.1:7788'
const DAEMON_WS = DAEMON.replace(/^http/, 'ws')

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  server: {
    // The daemon rejects requests whose `Host` is not `127.0.0.1:<port>` / `localhost:<port>`
    // (docs/API.md §0), so the proxy must rewrite Host to the target: changeOrigin: true.
    proxy: {
      '/api': { target: DAEMON, changeOrigin: true },
      '/hook': { target: DAEMON, changeOrigin: true },
      '/ws': { target: DAEMON_WS, ws: true, changeOrigin: true },
    },
  },
  build: {
    // M8 embeds this with rust-embed.
    outDir: 'dist',
    emptyOutDir: true,
  },
})
