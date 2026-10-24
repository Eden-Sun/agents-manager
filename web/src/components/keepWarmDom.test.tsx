/**
 * 主力的「保溫」在畫面上的兩個東西（真的掛元件進 happy-dom，後端是 `MockTransport`，走正式的 fetch 路徑）：
 * - 「壓縮」鈕旁的「不用保溫」開關：只主力的 claude 有（codex 的 cache 不會冷）、顯示目前狀態、再按一次取消。
 * - 保溫回覆到了：主力晶片帶 ♨ 標記（`keep-warm-replied`），不畫紫色外框；使用者送出新 prompt 才恢復；保溫回覆不亮未讀。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import type { FakeRequest } from '../testing/domHarness'
import { act, click, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { advanceMockTime, sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { BotStatusCard, type ChipHints } from './BotStatusCard'
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
    await advanceMockTime(3000) // mock 的啟動要 2.5 秒虛擬時間
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

test('「不用保溫」：主力的 codex 不畫（cache 不會冷、不保溫），daemon 回 400 not_primary', async () => {
  const { id } = await running('am-codex')
  assert.equal(useStore.getState().bots.find((b) => b.id === id)?.primary, true, 'mock 的 am-codex 是主力')
  await mount(<KeepWarmSkipButton botId={id} />)
  assert.equal(btn(), null)
  await assert.rejects(mock.request('POST', `/bots/${id}/keep-warm/skip`, { skip: true }), (e: { status?: number; body?: { error?: string } }) => e.status === 400 && e.body?.error === 'not_primary')
})

for (const [name, touch] of [['am-claude', false], ['am-claude', true]] as const) {
  test(`保溫按鈕（${name}、touch=${touch}）：跟隨 ♨，停止與取消仍切換 skip`, async () => {
    const { id, requests } = await running(name)
    await mount(<KeepWarmSkipButton botId={id} touch={touch} />)
    await mount(<UnreadChip />)
    assert.equal(btn()!.textContent, '不用保溫')

    mock.simulateKeepWarmReply(id)
    await act(() => useStore.getState().refreshState())
    assert.ok(chip(id)!.querySelector('.unread-chip-warm'))
    assert.equal(btn()!.textContent, '保溫中・停止保溫')
    assert.match(btn()!.title, /保溫回覆已到、快取仍熱/)
    assert.equal(btn()!.getAttribute('aria-pressed'), 'false', 'aria-pressed 仍代表停用保溫')

    await click(btn()!)
    await until(() => btn()?.getAttribute('aria-pressed') === 'true', '停止保溫')
    assert.equal(btn()!.textContent, '不保溫中・取消', '停用文案優先於仍在的保溫回覆')
    await click(btn()!)
    await until(() => btn()?.getAttribute('aria-pressed') === 'false', '取消停止')
    assert.equal(btn()!.textContent, '保溫中・停止保溫')
    assert.deepEqual(skipBodies(requests), [{ skip: true }, { skip: false }])

    mock.setCacheAge(id, 65)
    await act(() => useStore.getState().refreshState())
    assert.equal(chip(id)!.querySelector('.unread-chip-warm'), null)
    assert.equal(btn()!.textContent, '不用保溫', '過 TTL 後不再顯示保溫中')

    mock.simulateKeepWarmReply(id)
    await act(() => useStore.getState().refreshState())
    assert.equal(btn()!.textContent, '保溫中・停止保溫')
    await mock.request('POST', `/bots/${id}/prompt`, { text: '新活動', client_request_id: `warm-button-${id}-${touch}` })
    await act(() => useStore.getState().refreshState())
    assert.equal(btn()!.textContent, '不用保溫', '新 prompt 清除 ♨ 時同步回復')
    await advanceMockTime(3000) // mock 的回合要 2.4 秒虛擬時間
    await until(async () => {
      await act(() => useStore.getState().refreshState())
      return useStore.getState().runs[id]?.agent_status === 'idle'
    }, '新活動回合完成，不留給下一個測試')
  })
}

test('保溫按鈕：沒有 run 廣播時也在 TTL 到期後更新', async () => {
  const { id } = await running('am-claude')
  const realNow = Date.now
  const realInterval = window.setInterval
  const original = useStore.getState().runs[id]!
  const ticks: (() => void)[] = []
  window.setInterval = ((fn: () => void, ms: number) => {
    if (ms === 15_000) ticks.push(fn)
    return realInterval(fn, ms)
  }) as typeof window.setInterval
  try {
    const at = new Date().toISOString()
    await act(() => useStore.setState((s) => ({ runs: { ...s.runs, [id]: { ...original, keep_warm_replied_at: at, cache_kept_warm_at: at } } })))
    await mount(<KeepWarmSkipButton botId={id} />)
    assert.equal(btn()!.textContent, '保溫中・停止保溫')
    assert.ok(ticks.length > 0)
    const later = realNow() + 65 * 60_000
    Date.now = () => later
    await act(() => ticks.forEach((tick) => tick()))
    assert.equal(btn()!.textContent, '不用保溫')
  } finally {
    Date.now = realNow
    window.setInterval = realInterval
    await act(() => useStore.setState((s) => ({ runs: { ...s.runs, [id]: original } })))
  }
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

/** 冰凍（不用保溫）：主力晶片畫雪花（inline SVG），tooltip 講 cache 到期前自動壓縮；取消後雪花消失。 */
test('冰凍：按了「不用保溫」的主力晶片畫雪花圖示，取消後消失', async () => {
  const { id } = await running('am-claude')
  await act(() => useStore.getState().refreshState())
  await act(() => useStore.setState({ botUnread: {}, selectedBotId: null }))
  await mount(<UnreadChip />)
  assert.equal(chip(id)!.querySelector('.frozen-badge'), null, '一開始沒有雪花')

  await mock.request('POST', `/bots/${id}/keep-warm/skip`, { skip: true })
  await act(() => useStore.getState().refreshState())
  await until(() => chip(id)!.querySelector('.frozen-badge') !== null, '按了不用保溫就畫雪花')
  assert.ok(chip(id)!.querySelector('.frozen-badge svg'), '用 inline SVG，不用 emoji（#1180）')
  assert.equal(chip(id)!.querySelector('.frozen-badge')!.getAttribute('aria-hidden'), 'true', '文字在 tooltip 與 sr-only')
  assert.match(chip(id)!.title, /不保溫：cache 到期前會自動壓縮（context 夠大時）/)
  assert.match(chip(id)!.querySelector('.sr-only')!.textContent ?? '', /（不保溫）/)
  assert.equal(chip(id)!.querySelector('.unread-chip-warm'), null, '冰凍跟 ♨ 是不同的記號')

  await mock.request('POST', `/bots/${id}/keep-warm/skip`, { skip: false })
  await act(() => useStore.getState().refreshState())
  await until(() => chip(id)!.querySelector('.frozen-badge') === null, '取消後雪花消失')
})

/** 手機寬度：`matchMedia` 對 640px 斷點回 true。 */
function asPhone(): () => void {
  const original = window.matchMedia
  window.matchMedia = ((q: string) => ({ matches: q.includes('max-width: 640px'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  return () => (window.matchMedia = original)
}

const HINTS: ChipHints = { current: false, unread: 0, needsReply: false, waitsKids: false, kidsRunning: 0, cacheTitle: null }

test('手機版：「不用保溫」在主力的狀態卡裡（長按主力晶片開），可切換', async () => {
  const restore = asPhone()
  try {
    const { id, requests } = await running('am-claude')
    await mount(<BotStatusCard botId={id} hints={HINTS} anchor={null} onClose={() => {}} onLegend={() => {}} />)
    await until(() => btn() !== null, '狀態卡裡出現「不用保溫」')
    assert.ok(document.querySelector('.bot-status-card')!.contains(btn()!), '在卡片裡面')
    assert.equal(document.querySelectorAll('.keep-warm-skip-btn').length, 1)
    assert.ok(btn()!.classList.contains('touch'), '觸控版（CSS 給 ≥ 40px）')
    assert.equal(btn()!.textContent, '不用保溫')

    await click(btn()!)
    await until(() => btn()?.getAttribute('aria-pressed') === 'true', '開著')
    assert.equal(btn()!.textContent, '不保溫中・取消')
    assert.deepEqual(skipBodies(requests), [{ skip: true }])
    assert.equal(useStore.getState().runs[id]?.keep_warm_skip, true)
    assert.ok(document.querySelector('.bot-status-card'), '按了不關卡')

    await click(btn()!)
    await until(() => btn()?.getAttribute('aria-pressed') === 'false', '已取消')
    assert.deepEqual(skipBodies(requests), [{ skip: true }, { skip: false }])
  } finally {
    restore()
  }
})

test('手機版：Git／專案資訊彈窗裡不再有「不用保溫」', async () => {
  const restore = asPhone()
  try {
    const { id } = await running('am-claude')
    await act(() => useStore.setState({ selectedBotId: id }))
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    await click(document.querySelector<HTMLButtonElement>('.mobile-git-info')!)
    await until(() => document.querySelector('.context-bar') !== null, '彈窗開了')
    assert.equal(document.querySelector('.mobile-keep-warm-row'), null)
    assert.equal(document.querySelector('.keep-warm-skip-btn'), null, '入口只在狀態卡裡')
  } finally {
    restore()
  }
})

test('手機版：沒釘成主力，狀態卡裡沒有「不用保溫」', async () => {
  const restore = asPhone()
  try {
    const { id } = await running('am-claude')
    await mock.request('PATCH', `/bots/${id}`, { primary: false })
    await useStore.getState().refreshState()
    await mount(<BotStatusCard botId={id} hints={HINTS} anchor={null} onClose={() => {}} onLegend={() => {}} />)
    assert.ok(document.querySelector('.bot-status-card'), '卡片照樣開')
    assert.equal(btn(), null)
    await mock.request('PATCH', `/bots/${id}`, { primary: true })
  } finally {
    restore()
  }
})
