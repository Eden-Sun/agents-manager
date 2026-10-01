/** mock 的「最近刪除」要跟 `daemon/src/deleted_bots.rs`／`restore_bot` 同形（#757）：刪了會出現在清單、復原後回到 state。 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'
import { ApiError } from './types.ts'

interface Deleted {
  bots: { id: string; name: string; project_label: string; deleted_at: string }[]
}
const names = async (m: MockTransport) => ((await m.request('GET', '/bots/deleted')) as Deleted).bots.map((b) => b.name)
const stateBots = async (m: MockTransport) =>
  ((await m.request('GET', '/state')) as { projects: { bots: { id: string; name: string }[] }[] }).projects.flatMap((p) => p.bots)

test('刪掉的 bot 進清單（最新在前），復原後離開清單、回到 state', async () => {
  const m = new MockTransport()
  const before = await names(m)
  const victim = (await stateBots(m))[0]
  await m.request('DELETE', `/bots/${victim.id}`)
  assert.equal((await names(m))[0], victim.name)
  assert.equal((await names(m)).length, before.length + 1)
  assert.ok(!(await stateBots(m)).some((b) => b.id === victim.id))

  await m.request('POST', `/bots/${victim.id}/restore`)
  assert.ok((await stateBots(m)).some((b) => b.id === victim.id))
  assert.ok(!(await names(m)).includes(victim.name))
})

test('復原：沒刪的 409、不存在 404、同專案同名 409', async () => {
  const m = new MockTransport()
  const live = (await stateBots(m))[0]
  await assert.rejects(m.request('POST', `/bots/${live.id}/restore`), (e) => e instanceof ApiError && e.status === 409)
  await assert.rejects(m.request('POST', '/bots/nope/restore'), (e) => e instanceof ApiError && e.status === 404)
  await m.request('DELETE', `/bots/${live.id}`)
  const dup = await m.request('POST', `/projects/${((await m.request('GET', '/state')) as { projects: { id: string }[] }).projects[0].id}/bots`, { name: live.name, kind: 'claude' })
  assert.ok(dup)
  await assert.rejects(m.request('POST', `/bots/${live.id}/restore`), (e) => e instanceof ApiError && e.status === 409)
})
