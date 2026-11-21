import test from 'node:test'
import assert from 'node:assert/strict'
import { DEV_ALLOWED_HOSTS, devHostAllowed } from './devHosts.ts'

test('使用者平常的連法都放行：IP、localhost、tailnet 名字、短名', () => {
  for (const h of ['192.168.1.46:5173', '100.64.0.1:5173', '127.0.0.1:5173', 'localhost:5173', '[::1]:5173', '[fd7a:115c:a1e0::1]:5173', 'agm:5173', 'agm.example.ts.net:5173', 'AGM.EXAMPLE.TS.NET']) {
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
