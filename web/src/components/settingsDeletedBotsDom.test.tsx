/**
 * 環境設定「最近刪除」（DeletedBotsPanel，#757）：軟刪的 bot 列出來、一鍵復原。真的掛進 happy-dom，後端是 mock。
 * 復原成功＝那列消失、bot 回到側欄的清單；撞名（409）／讀不到清單要講清楚、不能留下半套狀態；別的分頁刪／復原之後這份清單要跟上。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import type { FakeRequest } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { DeletedBotsPanel } from './DeletedBotsPanel'

const mock = sharedMock

virtualMockTime()
afterEach(async () => {
  await unmountAll()
  useStore.setState({ notices: [] })
})
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)
const rows = () => [...document.querySelectorAll('.deleted-row')]
const rowOf = (name: string) => rows().find((r) => r.querySelector('.deleted-name')?.textContent === name)
const names = () => rows().map((r) => r.querySelector('.deleted-name')?.textContent)

async function open(): Promise<FakeRequest[]> {
  const requests = mockApi(mock)
  await useStore.getState().refreshState()
  await mount(<DeletedBotsPanel />)
  await until(() => rows().length > 0, '清單出現')
  return requests
}

it('列出軟刪的 bot：名稱、所屬專案、刪除時間', async () => {
  await open()
  assert.ok(names().includes('am-old-review') && names().includes('scratch-claude'))
  const meta = rowOf('am-old-review')!.querySelector('.deleted-meta')!.textContent ?? ''
  assert.match(meta, /刪除於 1 小時前/)
  assert.match(rowOf('scratch-claude')!.querySelector('.deleted-meta')!.textContent ?? '', /2 天前/)
})

it('復原：送 restore、那列消失、bot 回到 store；另一列不動', async () => {
  const requests = await open()
  assert.equal(useStore.getState().bots.some((b) => b.name === 'am-old-review'), false)
  await click(rowOf('am-old-review')!.querySelector('button.deleted-restore')!)
  await until(() => requests.some((r) => r.method === 'POST' && /\/bots\/[^/]+\/restore$/.test(r.path)), '送了 restore')
  await until(() => !rowOf('am-old-review'), '那列消失')
  assert.ok(useStore.getState().bots.some((b) => b.name === 'am-old-review'), 'bot 回到清單')
  assert.ok(rowOf('scratch-claude'), '另一列還在')
  assert.equal(rowOf('scratch-claude')!.querySelector<HTMLButtonElement>('button.deleted-restore')!.disabled, false, 'busy 放掉')
})

it('復原撞名（409）：跳「復原失敗」通知，那列留著、按鈕回到可按', async () => {
  await open()
  const gone = (await mock.request('GET', '/bots/deleted')) as { bots: { id: string; name: string; project_id: string }[] }
  const target = gone.bots.find((b) => b.name === 'scratch-claude')!
  await mock.request('POST', `/projects/${target.project_id}/bots`, { name: 'scratch-claude', kind: 'claude' })
  await useStore.getState().refreshState()
  await until(() => Boolean(rowOf('scratch-claude')), '清單重抓後列還在')
  await click(rowOf('scratch-claude')!.querySelector('button.deleted-restore')!)
  await until(() => useStore.getState().notices.some((n) => n.kind === 'error' && /復原失敗/.test(n.text)), '復原失敗通知')
  assert.match(useStore.getState().notices.find((n) => n.kind === 'error')!.text, /409/)
  assert.ok(rowOf('scratch-claude'), '列還在')
  assert.equal(rowOf('scratch-claude')!.querySelector<HTMLButtonElement>('button.deleted-restore')!.disabled, false)
})

it('別的分頁刪了一顆／復原了一顆：這份清單隨 state 更新跟上', async () => {
  await open()
  const live = useStore.getState().bots.find((b) => !b.parent_bot_id && b.name === 'am-claude-kid') ?? useStore.getState().bots[0]
  await mock.request('DELETE', `/bots/${live.id}`)
  await useStore.getState().refreshState()
  await until(() => Boolean(rowOf(live.name)), '別的分頁刪掉的出現在清單')
  await mock.request('POST', `/bots/${live.id}/restore`)
  await useStore.getState().refreshState()
  await until(() => !rowOf(live.name), '別的分頁復原後從清單消失')
})

it('讀不到清單：顯示提示，不是空白也不是「沒有已刪除的 Bot」', async () => {
  mockApi(mock)
  const realFetch = globalThis.fetch
  globalThis.fetch = (async (input: string, init?: { method?: string }) => {
    if (String(input).endsWith('/bots/deleted')) return new Response(JSON.stringify({ error: 'boom' }), { status: 500 })
    return (realFetch as (a: string, b?: unknown) => Promise<Response>)(input, init)
  }) as unknown as typeof fetch
  try {
    await mount(<DeletedBotsPanel />)
    await until(() => /讀不到已刪除的清單/.test(document.body.textContent ?? ''), '失敗提示')
    assert.doesNotMatch(document.body.textContent ?? '', /沒有已刪除的 Bot/)
  } finally {
    globalThis.fetch = realFetch
  }
})
