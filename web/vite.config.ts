import react from '@vitejs/plugin-react'
import { defineConfig, type ProxyOptions } from 'vite'

// Dev-time daemon endpoint (SPEC §5: `[server] listen = "127.0.0.1:7788"`).
const DAEMON = process.env.VITE_DAEMON ?? 'http://127.0.0.1:7788'
const DAEMON_WS = DAEMON.replace(/^http/, 'ws')

// The daemon only ever binds 127.0.0.1 (never the LAN), and its own Origin check stays
// strict to that on purpose. `vite --host` is what's reachable from other LAN devices, so a
// browser there sends e.g. `Origin: http://192.168.1.51:5173`. changeOrigin only rewrites
// `Host`, not `Origin`, so pin Origin to the daemon's own address here — the daemon never
// needs to trust anything beyond localhost.
const rewriteOrigin: ProxyOptions['configure'] = proxy => {
  const set = (req: { setHeader: (k: string, v: string) => void }) => req.setHeader('origin', DAEMON)
  proxy.on('proxyReq', (proxyReq) => set(proxyReq))
  proxy.on('proxyReqWs', (proxyReq) => set(proxyReq))
}

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  server: {
    // The daemon rejects requests whose `Host` is not `127.0.0.1:<port>` / `localhost:<port>`
    // (docs/API.md §0), so the proxy must rewrite Host to the target: changeOrigin: true.
    proxy: {
      '/api': { target: DAEMON, changeOrigin: true, configure: rewriteOrigin },
      '/hook': { target: DAEMON, changeOrigin: true, configure: rewriteOrigin },
      '/ws': { target: DAEMON_WS, ws: true, changeOrigin: true, configure: rewriteOrigin },
    },
  },
  build: {
    // M8 embeds this with rust-embed.
    outDir: 'dist',
    emptyOutDir: true,
  },
})
