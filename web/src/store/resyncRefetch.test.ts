/**
 * `resync`／重連之後，只靠 WS 事件維持的資料也要整份重拉：事件漏掉就沒有別的來源會補。
 * 身分停用（`identity_prefs_changed`）以前整個分頁只在第一次 `refreshState` 抓一次。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { requests, reset, routeDaemon } from './storeEnv.harness.ts'
import type { Bot, Project } from '../api/types.ts'

const { useStore, dispatchFrameForTest } = await import('./store.ts')
const { identityPrefKey } = await import('../api/index.ts')

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })
const settle = () => new Promise((r) => setTimeout(r, 40))
const project = () => ({ id: 'p1', label: 'p', path: '/p', host: 'local' }) as Project
const bot = (id: string) => ({ id, name: id, project_id: 'p1', kind: 'claude', identity: null }) as Bot

const stateBody = () => ({ daemon_seq: 1, projects: [{ ...project(), bots: [bot('b1')] }], bots: [bot('b1')], runs: [], turns: [] })

test('resync 之後身分停用名單整份重拉：別的分頁停用／恢復的、漏掉的事件都補上', async () => {
  reset()
  let disabled: { host: string; kind: string; identity: string }[] = []
  routeDaemon((r) => {
    if (r.path.endsWith('/state')) return json(stateBody())
    if (r.path.endsWith('/identity-prefs')) return json({ disabled })
    if (r.path.includes('/messages')) return json({ messages: [], turns: [], has_more: false })
    return json({})
  })
  await useStore.getState().refreshState()
  await settle()
  assert.deepEqual(useStore.getState().disabledIdentities, [], '第一次載入：沒有人被停用')

  // 斷線期間別的瀏覽器停用了 cc1，`identity_prefs_changed` 沒收到。
  disabled = [{ host: 'local', kind: 'claude', identity: 'cc1' }]
  requests.length = 0
  dispatchFrameForTest({ type: 'resync', seq: 5 })
  await settle()
  assert.ok(requests.some((r) => r.path.endsWith('/identity-prefs')), 'resync 要重拉 identity-prefs')
  assert.deepEqual(useStore.getState().disabledIdentities, [identityPrefKey('local', 'claude', 'cc1')])
})

test('第一次讀身分停用名單失敗：之後的快照刷新會再試，不是整個分頁都不再問', async () => {
  reset()
  useStore.setState({ disabledIdentities: [] })
  let fail = true
  routeDaemon((r) => {
    if (r.path.endsWith('/state')) return json(stateBody())
    if (r.path.endsWith('/identity-prefs')) return fail ? json({ error: 'boom' }, 500) : json({ disabled: [{ host: 'local', kind: 'claude', identity: 'cc2' }] })
    return json({})
  })
  await useStore.getState().loadIdentityPrefs()
  assert.deepEqual(useStore.getState().disabledIdentities, [])
  fail = false
  await useStore.getState().refreshState()
  await settle()
  assert.deepEqual(useStore.getState().disabledIdentities, [identityPrefKey('local', 'claude', 'cc2')])
})

test('resync 之後輸入框草稿整份重拉（lag 期間漏掉的 draft_updated 沒有別的來源）', async () => {
  reset()
  routeDaemon((r) => {
    if (r.path.endsWith('/state')) return json(stateBody())
    if (r.path.endsWith('/drafts')) return json({ drafts: [{ key: 'bot:b-resync', text: '別台打的', rev: 99999 }] })
    return json({ disabled: [], messages: [], turns: [], has_more: false })
  })
  useStore.setState({ drafts: {} })
  dispatchFrameForTest({ type: 'resync', seq: 5 })
  await settle()
  assert.ok(requests.some((r) => r.method === 'GET' && r.path.endsWith('/drafts')), 'resync 要重拉 drafts')
  assert.equal(useStore.getState().drafts['bot:b-resync'], '別台打的')
})

test('resync 之後總管 rev 與每顆 bot 的分享 rev 都加一：這兩份只靠事件計數，漏掉的事件沒有別的來源', async () => {
  reset()
  routeDaemon((r) => {
    if (r.path.endsWith('/state')) return json(stateBody())
    return json({ disabled: [], messages: [], turns: [], has_more: false })
  })
  await useStore.getState().refreshState()
  await settle()
  const sup = useStore.getState().supervisorRev
  const share = useStore.getState().shareRev.b1 ?? 0
  dispatchFrameForTest({ type: 'resync', seq: 5 })
  await settle()
  assert.equal(useStore.getState().supervisorRev, sup + 1)
  assert.equal(useStore.getState().shareRev.b1 ?? 0, share + 1)
})
