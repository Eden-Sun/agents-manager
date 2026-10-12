/**
 * 身分停用名單的請求可能重疊（重連、resync、按鈕各自觸發）。回來的順序不保證：較舊的那份晚到，
 * 不能把較新的蓋掉（同 #1489 的 pending-question 那類問題）。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { requests, reset, routeDaemon } from './storeEnv.harness.ts'

const { useStore } = await import('./store.ts')
const { identityPrefKey } = await import('../api/index.ts')

const json = (body: unknown) => new Response(JSON.stringify(body), { status: 200 })

test('兩次身分停用請求回來的順序顛倒：較舊的那份晚到，不能蓋掉較新的', async () => {
  reset()
  const release: Array<() => void> = []
  routeDaemon((r) => {
    if (!r.path.endsWith('/identity-prefs')) return json({})
    // 第一次（舊）的回應先不放，等第二次（新）回來後才放。
    return new Promise<Response>((resolve) => {
      const idx = release.length
      release.push(() =>
        resolve(json({ disabled: idx === 0 ? [{ host: 'local', kind: 'claude', identity: 'old' }] : [{ host: 'local', kind: 'claude', identity: 'new' }] })),
      )
    })
  })
  requests.length = 0
  const first = useStore.getState().loadIdentityPrefs()
  const second = useStore.getState().loadIdentityPrefs()
  await new Promise((r) => setTimeout(r, 5))
  assert.equal(release.length, 2, '兩次都要真的打出去')
  release[1]()
  await second
  release[0]()
  await first
  assert.deepEqual(useStore.getState().disabledIdentities, [identityPrefKey('local', 'claude', 'new')])
})
