/**
 * mock 的 pane 白名單要跟 daemon 的 `shell::registered` 一樣一直成立（第二輪 review M6）：
 * 以前被 trace 的 pane 看過一次就塞進「自己開的 shell」，之後打字不再 403、`GET …/shells` 還把它列出來，
 * 按「開 shell」就接到 dev server 那顆；而且每個專案底下都列同一組 pane，M4 的重複在 mock 下看不出來。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'
import { ApiError } from './types.ts'

interface Row {
  pane_id: string
  project_id: string | null
  read_only: boolean
  scratch?: boolean
}

const mock = new MockTransport()
const projectId = async () => ((await mock.request('GET', '/state')) as { projects: { id: string }[] }).projects[0].id
const status = async (p: Promise<unknown>) => {
  try {
    await p
    return 200
  } catch (e) {
    return e instanceof ApiError ? e.status : -1
  }
}

test('有 port 的 pane 看過畫面之後，打字照樣 403，也不會被列成自己開的 shell', async () => {
  await projectId()
  const typeKeys = () => mock.request('POST', '/hosts/local/shells/w168%3Ap62/keys', { keys: ['ctrl+c'] })
  assert.equal(await status(mock.request('GET', '/hosts/local/shells/w168%3Ap62/terminal?source=visible&lines=50')), 200)
  assert.equal(await status(typeKeys()), 403)
  assert.equal(await status(typeKeys()), 403, '第二次也一樣')
  const own = (await mock.request('GET', '/hosts/local/shells')) as { shells: { pane_id: string }[] }
  assert.ok(!own.shells.some((s) => s.pane_id === 'w168:p62'), '被 trace 的 pane 不是面板自己開的')
})

test('跑著 vim、沒有 port 的 pane：唯讀是 false，打得進去', async () => {
  const all = (await mock.request('GET', '/panes')) as { panes: Row[] }
  assert.equal(all.panes.find((p) => p.pane_id === 'w168:p63')?.read_only, false)
  assert.equal(await status(mock.request('POST', '/hosts/local/shells/w168%3Ap63/text', { text: ':q', enter: true })), 200)
})

test('專案只回自己的 pane；沒歸屬的只在 ?unowned=1，並標出 scratch', async () => {
  const pid = await projectId()
  const mine = (await mock.request('GET', `/projects/${pid}/panes`)) as { panes: Row[] }
  assert.ok(mine.panes.length > 0)
  assert.ok(mine.panes.every((p) => p.project_id === pid))
  assert.deepEqual(((await mock.request('GET', '/projects/other/panes')) as { panes: Row[] }).panes, [])
  const unowned = (await mock.request('GET', '/panes?unowned=1')) as { panes: Row[] }
  assert.ok(unowned.panes.every((p) => p.project_id === null))
  assert.equal(unowned.panes.filter((p) => p.scratch).length, 1, '只有一顆 scratch')
})
