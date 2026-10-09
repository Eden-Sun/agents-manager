/**
 * 主力的「保溫」在畫面上的兩個東西（真的掛元件進 happy-dom，後端是 `MockTransport`，走正式的 fetch 路徑）：
 * - 「壓縮」鈕旁的「不用保溫」開關：只主力的 claude／codex 有、顯示目前狀態、再按一次取消。
 * - 保溫回覆到了：主力晶片帶 ♨ 標記（`keep-warm-replied`），不畫紫色外框；使用者送出新 prompt 才恢復；保溫回覆不亮未讀。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import type { FakeRequest } from '../testing/domHarness'
import { act, click, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { ChatPanel } from './ChatPanel'
import { KeepWarmSkipButton } from './KeepWarmSkipButton'
import { UnreadChip } from './UnreadChip'

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const mock = sharedMock
const btn = () => document.querySelector<HTMLButtonElement>('.keep-warm-skip-btn')
const chip = (id: string) => document.querySelector<HTMLElement>(`.unread-chip[data-bot-id="${id}"]`)

/** 起 `name` 並等它 running；回 bot id。 */
async function running(name: string): Promise<{ id: string; requests: FakeRequest[] }> {
  const requests = mockApi(mock)
  await useStore.getState().refreshState()
  const bot = useStore.getState().bots.find((b) => b.name === name)!
  assert.ok(bot, `mock 要有 ${name}`)
  if (useStore.getState().runs[bot.id]?.state !== 'running') {
    await mock.request('POST', `/bots/${bot.id}/start`)
    await until(async () => {
      await useStore.getState().refreshState()
      return useStore.getState().runs[bot.id]?.state === 'running'
    }, `${name} running`)
  }
  requests.length = 0
  return { id: bot.id, requests }
}

const skipBodies = (requests: FakeRequest[]) => requests.filter((r) => r.method === 'POST' && r.path.endsWith('/keep-warm/skip')).map((r) => r.body)

test('「不用保溫」：主力 claude 顯示目前狀態、按一下開、再按一下取消', async () => {
  const { id, requests } = await running('am-claude')
  await mount(<KeepWarmSkipButton botId={id} />)
  assert.equal(btn()!.textContent, '不用保溫')
  assert.equal(btn()!.getAttribute('aria-pressed'), 'false')

  await click(btn()!)
  await until(() => btn()?.getAttribute('aria-pressed') === 'true', '開著')
  assert.equal(btn()!.textContent, '不保溫中・取消', '文字講出目前狀態，不只靠顏色')
  assert.ok(btn()!.classList.contains('on'))
  assert.deepEqual(skipBodies(requests), [{ skip: true }])
  assert.equal(useStore.getState().runs[id]?.keep_warm_skip, true)

  await click(btn()!)
  await until(() => btn()?.getAttribute('aria-pressed') === 'false', '已取消')
  assert.equal(btn()!.textContent, '不用保溫')
  assert.deepEqual(skipBodies(requests), [{ skip: true }, { skip: false }])
  assert.equal(useStore.getState().runs[id]?.keep_warm_skip, false)
})

test('「不用保溫」：沒釘成主力就不畫；daemon 對非主力回 400 not_primary', async () => {
  const { id } = await running('am-claude')
  await mock.request('PATCH', `/bots/${id}`, { primary: false })
  await useStore.getState().refreshState()
  await mount(<KeepWarmSkipButton botId={id} />)
  assert.equal(btn(), null, '沒釘成主力就沒有這顆鈕')
  await assert.rejects(mock.request('POST', `/bots/${id}/keep-warm/skip`, { skip: true }), (e: { status?: number; body?: { error?: string } }) => e.status === 400 && e.body?.error === 'not_primary')
  await mock.request('PATCH', `/bots/${id}`, { primary: true })
})

test('保溫回覆：TTL 內帶 ♨ 提示、過 TTL 消失、送 prompt 立即消失', async () => {
  const { id } = await running('am-claude')
  await act(() => useStore.getState().refreshState())
  await act(() => useStore.setState({ botUnread: {}, selectedBotId: null }))
  await mount(<UnreadChip />)
  assert.ok(chip(id), '主力一定有晶片')
  assert.ok(!chip(id)!.classList.contains('keep-warm-replied'), '一開始沒提示')

  // 1) 保溫後 TTL 內帶 ♨ 提示：晶片帶 ♨ 符號，不畫紫色外框，不亮未讀
  mock.simulateKeepWarmReply(id)
  await act(() => useStore.getState().refreshState())
  await until(() => chip(id)!.classList.contains('keep-warm-replied'), '保溫後 TTL 內晶片標記 keep-warm-replied')
  assert.ok(chip(id)!.querySelector('.unread-chip-warm'), '帶有小 ♨ 記號')
  assert.match(chip(id)!.querySelector('.sr-only')!.textContent ?? '', /保溫回覆已到/)
  assert.equal(chip(id)!.classList.contains('unread'), false, '保溫回覆不亮未讀')
  assert.equal(useStore.getState().botUnread[id] ?? 0, 0)
  // 顏色不跟快取倒數的底色 class、未讀、要回答撞：它是獨立的 class，倒數底色照樣在。
  assert.ok(![...chip(id)!.classList].some((c) => c === 'unread' || c === 'needs-reply'))

  // 2) 過 TTL 提示消失：快取已涼（65 分鐘前，超過 3600 秒 TTL），即使 keep_warm_replied_at 仍在，♨ 也消失
  mock.setCacheAge(id, 65)
  await act(() => useStore.getState().refreshState())
  await until(() => !chip(id)!.classList.contains('keep-warm-replied'), '過 TTL 後提示消失')
  assert.equal(chip(id)!.querySelector('.unread-chip-warm'), null, '過 TTL 後 ♨ 消失')

  // 重新演一輪保溫回覆，讓提示再次出現
  mock.simulateKeepWarmReply(id)
  await act(() => useStore.getState().refreshState())
  await until(() => chip(id)!.classList.contains('keep-warm-replied'), '再次保溫後帶有提示')
  assert.ok(chip(id)!.querySelector('.unread-chip-warm'), '再次保溫後有 ♨')

  // 3) 送 prompt 立即取消提示：使用者送 prompt 後 daemon 把 keep_warm_replied_at 清為 null
  await mock.request('POST', `/bots/${id}/prompt`, { text: '真的 prompt', client_request_id: `real-${Date.now()}` })
  await act(() => useStore.getState().refreshState())
  await until(() => !chip(id)!.classList.contains('keep-warm-replied'), '送 prompt 後提示立即恢復')
  assert.equal(chip(id)!.querySelector('.unread-chip-warm'), null, '送 prompt 後 ♨ 消失')
})


/** 手機寬度：`matchMedia` 對 640px 斷點回 true。 */
function asPhone(): () => void {
  const original = window.matchMedia
  window.matchMedia = ((q: string) => ({ matches: q.includes('max-width: 640px'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  return () => (window.matchMedia = original)
}

test('手機版：「Git / 專案資訊」彈窗最上面有「不用保溫」列，可切換；狀態列裡不重複', async () => {
  const restore = asPhone()
  try {
    const { id, requests } = await running('am-claude')
    await act(() => useStore.setState({ selectedBotId: id }))
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    assert.equal(document.querySelector('.mobile-keep-warm-row'), null, '彈窗沒開就看不到')
    await click(document.querySelector<HTMLButtonElement>('.mobile-git-info')!)
    await until(() => document.querySelector('.mobile-keep-warm-row') !== null, '開彈窗後出現「不用保溫」列')
    assert.equal(document.querySelectorAll('.keep-warm-skip-btn').length, 1, '手機只有彈窗那一顆，狀態列不重複')
    assert.ok(btn()!.classList.contains('touch'), '觸控版（CSS 給 ≥ 40px）')
    assert.equal(btn()!.textContent, '不用保溫')

    await click(btn()!)
    await until(() => btn()?.getAttribute('aria-pressed') === 'true', '開著')
    assert.equal(btn()!.textContent, '不保溫中・取消')
    assert.deepEqual(skipBodies(requests), [{ skip: true }])
    assert.equal(useStore.getState().runs[id]?.keep_warm_skip, true)

    await click(btn()!)
    await until(() => btn()?.getAttribute('aria-pressed') === 'false', '已取消')
    assert.deepEqual(skipBodies(requests), [{ skip: true }, { skip: false }])
  } finally {
    restore()
  }
})

test('手機版：沒釘成主力就沒有「不用保溫」列', async () => {
  const restore = asPhone()
  try {
    const { id } = await running('am-claude')
    await mock.request('PATCH', `/bots/${id}`, { primary: false })
    await useStore.getState().refreshState()
    await act(() => useStore.setState({ selectedBotId: id }))
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    await click(document.querySelector<HTMLButtonElement>('.mobile-git-info')!)
    assert.equal(document.querySelector('.mobile-keep-warm-row'), null)
    await mock.request('PATCH', `/bots/${id}`, { primary: true })
  } finally {
    restore()
  }
})
