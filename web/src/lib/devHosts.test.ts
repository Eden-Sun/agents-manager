import test from 'node:test'
import assert from 'node:assert/strict'
import { DEV_ALLOWED_HOSTS, devHostAllowed } from './devHosts.ts'

// vite 的 `server.allowedHosts` 只管一般 HTTP；proxy 的 WebSocket upgrade 不經過那道檢查（實測：Host: evil.example 的
// upgrade 照樣被轉給 daemon，而 proxy 又把 Host／Origin 改寫成 daemon 自己的位址，daemon 的 Host 檢查等於被繞過）。
// vite.config.ts 在 `/ws` 的 `bypass` 用這支函式補上同一條規則。

test('使用者平常的連法都放行：IP、localhost、tailnet 名字、短名', () => {
  for (const h of ['192.168.1.46:5173', '100.80.47.75:5173', '127.0.0.1:5173', 'localhost:5173', '[::1]:5173', '[fd7a:115c:a1e0::1]:5173', 'agm:5173', 'agm.tail161aae.ts.net:5173', 'AGM.TAIL161AAE.TS.NET']) {
    assert.equal(devHostAllowed(h, DEV_ALLOWED_HOSTS), true, h)
  }
})

test('DNS rebinding 用的名字一律擋', () => {
  for (const h of ['evil.example', 'evil.example:5173', '127.0.0.1.evil.example', '192.168.1.46.nip.io', 'x.ts.net.evil.example', 'agm.evil.example', 'agm-host', 'x.local', '', undefined]) {
    assert.equal(devHostAllowed(h, DEV_ALLOWED_HOSTS), false, String(h))
  }
})

test('. 開頭的條目比對整個後綴（跟 vite 的 allowedHosts 同義）', () => {
  assert.equal(devHostAllowed('a.b.ts.net', ['.ts.net']), true)
  assert.equal(devHostAllowed('ts.net', ['.ts.net']), true)
  assert.equal(devHostAllowed('notts.net', ['.ts.net']), false)
})
