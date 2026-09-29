import test from 'node:test'
import assert from 'node:assert/strict'
import { applyUpstreamItem, loadUpstreamUpdates, parseUpstreamItem, upstreamNotice } from './upstreamUpdate.ts'

const store = new Map<string, string>()
;(globalThis as { localStorage?: unknown }).localStorage = {
  getItem: (k: string) => store.get(k) ?? null,
  setItem: (k: string, v: string) => void store.set(k, v),
  removeItem: (k: string) => void store.delete(k),
}

const CLAUDE = {
  kind: 'claude',
  latest_version: '2.1.283',
  target_version: '2.1.284',
  has_update: true,
  error: null,
  notify: 'update',
  hosts: [
    { host: 'local', installed_version: '2.1.284', error: null, behind: false },
    { host: 'm4p', installed_version: '2.1.281', error: null, behind: true },
  ],
  text: 'claude 需安裝 2.1.284（local：2.1.284 → 2.1.284；m4p：2.1.281 → 2.1.284）',
}

function recorder() {
  const got: { kind: string; text: string }[] = []
  return { got, notify: (kind: 'info' | 'error', text: string) => void got.push({ kind, text }) }
}

test('有新版跳一次，同一版同一個瀏覽器不再跳', () => {
  store.clear()
  const r = recorder()
  applyUpstreamItem(CLAUDE, r.notify)
  applyUpstreamItem({ ...CLAUDE, notify: null }, r.notify)
  assert.equal(r.got.length, 1)
  assert.equal(r.got[0].kind, 'info')
  assert.match(r.got[0].text, /2\.1\.284/)
  applyUpstreamItem({ ...CLAUDE, latest_version: '2.1.284', text: 'claude 上游有新版 2.1.284' }, r.notify)
  assert.equal(r.got.length, 2, '下一版要再跳')
})

test('target 與 fleet 主機快照會解析，且開機讀取可以更新持續狀態', async () => {
  const parsed = parseUpstreamItem(CLAUDE)
  assert.equal(parsed?.target, '2.1.284')
  assert.deepEqual(parsed?.hosts.map((host) => [host.host, host.installedVersion, host.behind]), [
    ['local', '2.1.284', false],
    ['m4p', '2.1.281', true],
  ])
  const saved: string[] = []
  await loadUpstreamUpdates(() => Promise.resolve({ items: [CLAUDE] }), () => {}, (item) => saved.push(item.kind))
  assert.deepEqual(saved, ['claude'])
})

test('沒有新版不跳；抓不到上游只在 daemon 推 error 的那一次跳', () => {
  store.clear()
  assert.equal(upstreamNotice(parseUpstreamItem({ kind: 'codex', latest_version: '0.157.0', has_update: false, text: null })!, {}), null)
  const err = { kind: 'claude', latest_version: null, has_update: false, notify: 'error', text: '查 claude 上游最新版失敗（npm registry）：timeout' }
  assert.deepEqual(upstreamNotice(parseUpstreamItem(err)!, {}), { kind: 'error', text: err.text })
  assert.equal(upstreamNotice(parseUpstreamItem({ ...err, notify: null })!, {}), null, '沒有 notify 的快照不重複講錯')
})

test('開機補讀：錯過推播的新版會跳，錯誤不會每次重整都跳', async () => {
  store.clear()
  const r = recorder()
  const err = { kind: 'codex', latest_version: null, has_update: false, notify: null, text: '查 codex 上游最新版失敗' }
  await loadUpstreamUpdates(() => Promise.resolve({ items: [{ ...CLAUDE, notify: null }, err] }), r.notify)
  assert.deepEqual(r.got.map((g) => g.kind), ['info'])
  await loadUpstreamUpdates(() => Promise.resolve({ items: [CLAUDE] }), r.notify)
  assert.equal(r.got.length, 1, '已看過的不再跳')
  await loadUpstreamUpdates(() => Promise.reject(new Error('404')), r.notify)
  assert.equal(r.got.length, 1, '舊 daemon 沒有這支不炸')
})
