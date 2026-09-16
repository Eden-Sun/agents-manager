/**
 * 開機時 `GET /api/session` 被擋下來，store 要分得出兩種情況：
 * daemon 真的連不上（給錯誤畫面），還是這台裝置只是還沒配對（給輸入配對碼的畫面，SPEC §7.1a）。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { requests, reset, routeDaemon } from './storeEnv.harness.ts'
import { TOKEN_KEY } from '../api/sessionToken.ts'
import * as api from '../api/index.ts'

const { useStore } = await import('./store.ts')

const json = (body: unknown, status: number) => new Response(JSON.stringify(body), { status })

function seed() {
  reset()
  // 每一段都從「這台裝置沒有 token」開始，不然 `session()` 根本不會去問 daemon。
  localStorage.removeItem(TOKEN_KEY)
  useStore.setState({ ready: false, bootError: null, needsPairing: false })
}

test('403 pairing_required 是「還沒配對」，不是「連不上 daemon」', async () => {
  seed()
  routeDaemon((r) => (r.path === '/api/session' ? json({ error: 'pairing_required' }, 403) : json({}, 500)))

  await useStore.getState().bootstrap()

  const s = useStore.getState()
  assert.equal(s.needsPairing, true)
  // 錯誤畫面要讓位給配對畫面，兩個一起亮等於在手機上同時說「壞了」跟「輸入碼」。
  assert.equal(s.bootError, null)
  assert.equal(s.ready, false)
})

test('其他失敗照舊走錯誤畫面，不會被誤判成要配對', async () => {
  seed()
  routeDaemon(() => json({ error: 'upstream', message: 'daemon 掛了' }, 502))

  await useStore.getState().bootstrap()

  const s = useStore.getState()
  assert.equal(s.needsPairing, false)
  assert.ok(s.bootError)
})

test('同樣是 403，Host/Origin 非本機不算還沒配對——那是設定問題', async () => {
  seed()
  routeDaemon(() => json({ error: 'non-local request' }, 403))

  await useStore.getState().bootstrap()

  assert.equal(useStore.getState().needsPairing, false)
  assert.ok(useStore.getState().bootError)
})

test('配對成功後 token 留在裝置上，開機不再問 /api/session', async () => {
  seed()
  routeDaemon((r) => {
    if (r.path === '/api/session') return json({ error: 'pairing_required' }, 403)
    if (r.path === '/api/session/pair') return json({ token: 'paired-token', port: 7788 }, 200)
    // 這一段只看配對那一步，state 讓它失敗以免開機一路走到 WebSocket。
    return json({ error: 'upstream' }, 502)
  })

  await useStore.getState().bootstrap()
  assert.equal(useStore.getState().needsPairing, true)

  await api.pairDevice('abc-def')
  assert.equal(localStorage.getItem(TOKEN_KEY), 'paired-token')
  assert.deepEqual(requests.at(-1), { method: 'POST', path: '/api/session/pair', body: { code: 'ABCDEF' } })

  requests.length = 0
  await useStore.getState().bootstrap()

  // 重新開機：不該再出現 `/api/session`，那條路在手機上只會再拿到一次 403。
  assert.equal(requests.filter((r) => r.path === '/api/session').length, 0)
  assert.equal(useStore.getState().needsPairing, false)
})
