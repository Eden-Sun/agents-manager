import react from '@vitejs/plugin-react'
import { defineConfig, type ProxyOptions } from 'vite'
import { DEV_ALLOWED_HOSTS, devHostAllowed } from './src/lib/devHosts.ts'

const DAEMON = process.env.VITE_DAEMON ?? 'http://127.0.0.1:7788'
const DAEMON_WS = DAEMON.replace(/^http/, 'ws')

const rewriteOrigin: ProxyOptions['configure'] = proxy => {
  const set = (req: { setHeader: (k: string, v: string) => void }) => req.setHeader('origin', DAEMON)
  proxy.on('proxyReq', (proxyReq) => set(proxyReq))
  proxy.on('proxyReqWs', (proxyReq) => set(proxyReq))
}

export default defineConfig({
  plugins: [react()],
  server: {
    host: true,
    allowedHosts: DEV_ALLOWED_HOSTS,
    proxy: {
      '/api': {
        target: DAEMON,
        ws: true,
        changeOrigin: true,
        configure: rewriteOrigin,
        bypass: req => (req.headers.upgrade && !devHostAllowed(req.headers.host, DEV_ALLOWED_HOSTS) ? false : undefined),
      },
      '/hook': { target: DAEMON, changeOrigin: true, configure: rewriteOrigin },
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
    outDir: 'dist',
    emptyOutDir: true,
    rolldownOptions: {
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
