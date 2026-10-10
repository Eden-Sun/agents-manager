import react from '@vitejs/plugin-react'
import { defineConfig, type ProxyOptions } from 'vite'
import { DEV_ALLOWED_HOSTS, devHostAllowed } from './src/lib/devHosts.ts'

// Dev-time daemon endpoint (SPEC §5: `[server] listen = "127.0.0.1:7788"`).
const DAEMON = process.env.VITE_DAEMON ?? 'http://127.0.0.1:7788'
const DAEMON_WS = DAEMON.replace(/^http/, 'ws')

// Daemon 的綁定與 Origin 檢查（#1076）：不是 .app 的執行方式（cargo dev、cargo run、手動跑的 binary）預設綁 0.0.0.0，
// 並放寬 Host／Origin（daemon/src/serve.rs `dev_lan_default`、api.rs `origin_is_local` 的 allow_lan）；只有打包的 .app 才綁
// 127.0.0.1。所以 5173 能連到的人，經這個 proxy 打進 daemon 時，daemon 看到的都是本機（peer 是 vite 的 loopback）。
// 安全邊界因此是 5173 自己的 `allowedHosts`（DEV_ALLOWED_HOSTS）與 UI token（X-AM-Token），不是 daemon 的 LAN 檢查。
// `vite --host` 讓 LAN 裝置連得上，它們送來的是 `Origin: http://192.168.1.51:5173`；changeOrigin 只改 `Host`、不改 `Origin`，
// 所以這裡把 Origin 改寫成 daemon 自己的位址，proxy 路徑在 daemon 只綁 localhost（打包版、AM_DEV_LAN=0）時也過得去。
const rewriteOrigin: ProxyOptions['configure'] = proxy => {
  const set = (req: { setHeader: (k: string, v: string) => void }) => req.setHeader('origin', DAEMON)
  proxy.on('proxyReq', (proxyReq) => set(proxyReq))
  proxy.on('proxyReqWs', (proxyReq) => set(proxyReq))
}

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  server: {
    // 使用者是從手機／LAN 上的裝置開這個 dev server，只綁 loopback 他們連不到，
    // 所以 5173 一律對外（SPEC §18.1）。dev 的 daemon 也綁 0.0.0.0（見檔頭），跨裝置來的請求
    // 靠下面 proxy 的 changeOrigin + Origin 改寫，以本機身分進 daemon。
    host: true,
    // vite 只放行 localhost 與 IP：用 tailnet 名字開（`http://agm:5173`、`agm.tail161aae.ts.net`）會被擋成 403
    // （2026-10-02 使用者）。只放行自己 tailnet 的名字，不整個關掉主機檢查（那是擋 DNS rebinding 的）。
    allowedHosts: DEV_ALLOWED_HOSTS,
    // The daemon rejects requests whose `Host` is not `127.0.0.1:<port>` / `localhost:<port>`
    // (docs/API.md §0), so the proxy must rewrite Host to the target: changeOrigin: true.
    proxy: {
      '/api': { target: DAEMON, changeOrigin: true, configure: rewriteOrigin },
      '/hook': { target: DAEMON, changeOrigin: true, configure: rewriteOrigin },
      // `allowedHosts` 只管一般 HTTP：proxy 的 WebSocket upgrade 不經過它，而 changeOrigin／rewriteOrigin 又把 Host／Origin
      // 改成 daemon 自己的位址，daemon 那一側的 Host 檢查等於被繞過。upgrade 這裡補上同一條規則（false＝直接斷線）。
      '/ws': {
        target: DAEMON_WS,
        ws: true,
        changeOrigin: true,
        configure: rewriteOrigin,
        bypass: req => (devHostAllowed(req.headers.host, DEV_ALLOWED_HOSTS) ? undefined : false),
      },
    },
  },
  build: {
    // M8 embeds this with rust-embed.
    outDir: 'dist',
    emptyOutDir: true,
    // React DOM is the largest single rendered module (~453 kB before final minification). Isolate it so the application
    // entry stays small; lazy Markdown keeps its parser and GFM extensions off the startup path.
    rolldownOptions: {
      // 分享頁（SPEC「分享 bot」）是獨立 entry：`dist/share.html` 只帶 `src/share/` 的程式碼，不帶主 UI。
      input: { main: 'index.html', share: 'share.html' },
      output: {
        codeSplitting: {
          groups: [{
            name: 'react-dom',
            test: /node_modules[\\/]react-dom[\\/]/,
            includeDependenciesRecursively: false,
          }, {
            name: 'initial-ui',
            test: /src[\\/]components[\\/]/,
            tags: ['$initial'],
            minSize: 20 * 1024,
            includeDependenciesRecursively: false,
          }],
        },
      },
    },
  },
})
