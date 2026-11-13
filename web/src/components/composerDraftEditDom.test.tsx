/**
 * 「編輯後送出」（2026-11-13，Session paused → Edit prompt and retry 之後網頁看不到也改不了 CLI 輸入框那段）：
 * 完整草稿（不是截短的 500 字）放進網頁輸入框，改完送出＝`clear_draft`＋`expect_draft_token`＋改過的 `text`；
 * Session paused 選了第 2 項之後自動帶進編輯；第 1 項（Switch to…）不動。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { act, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import type { FakeRequest } from '../testing/domHarness'
import { flush } from '../testing/flush'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import type { Run, TerminalSnapshot } from '../api/types.ts'
import { parseChoiceMenu } from '../lib/tuiChoices.ts'
import { dispatchFrameForTest, resetStoreForTest, useStore } from '../store/store'
import { ChatPanel } from './ChatPanel'
import { BlockedChoices } from './BlockedChoices.tsx'

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const mock = sharedMock
const SESSION_PAUSED = readFileSync(new URL('../lib/__fixtures__/claude-2.1.281-session-paused.txt', import.meta.url), 'utf8')
const LONG = `${'長'.repeat(700)}尾`
const prompts = (requests: FakeRequest[]) => requests.filter((r) => r.method === 'POST' && /\/bots\/[^/]+\/prompt/.test(r.path))
const buttons = () => [...document.querySelectorAll<HTMLButtonElement>('.composer-draft button')]
const byText = (s: string) => buttons().find((b) => b.textContent?.includes(s))
const textarea = () => document.querySelector<HTMLTextAreaElement>('.composer textarea')!

async function openChat(name: string) {
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
  useStore.getState().selectBot(bot.id)
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  await flush()
  // 前一個測試留下的草稿與那一條不能帶進來。
  await act(async () => {
    await useStore.getState().setDraft(`bot:${bot.id}`, '')
    useStore.getState().dismissComposerDraft(bot.id)
  })
  return { requests, bot }
}

test('主動讀框：GET composer-draft 拿到完整草稿（不截）並掛上那一條；空框什麼都不掛', { timeout: 30_000 }, async () => {
  const { bot } = await openChat('am-claude')
  mock.composerDrafts.set(bot.id, null)
  assert.equal(await useStore.getState().loadComposerDraft(bot.id), null)
  assert.equal(useStore.getState().composerDrafts[bot.id], undefined)
  mock.composerDrafts.set(bot.id, LONG)
  const got: { block: Awaited<ReturnType<ReturnType<typeof useStore.getState>['loadComposerDraft']>> } = { block: null }
  await act(async () => {
    got.block = await useStore.getState().loadComposerDraft(bot.id)
  })
  const block = got.block
  assert.ok(block)
  assert.equal(block.draft, LONG, '完整草稿')
  assert.equal(block.truncated, false)
  assert.deepEqual(block.actions, ['submit', 'clear'])
  await until(() => byText('編輯後送出') !== undefined, '輸入列上方出現三個動作')
  assert.ok(byText('送出框裡那段') && byText('編輯後送出') && byText('取消'))
  mock.composerDrafts.set(bot.id, null)
})

test('編輯後送出：完整草稿進輸入框，改完送出＝clear_draft＋token＋改過的 text', { timeout: 30_000 }, async () => {
  const { requests, bot } = await openChat('am-claude')
  mock.composerDrafts.set(bot.id, LONG)
  // 模擬 409 帶來的截短版（500 字），按下去要換成完整的。
  const full = await useStore.getState().loadComposerDraft(bot.id)
  assert.ok(full)
  useStore.setState((s) => ({
    composerDrafts: { ...s.composerDrafts, [bot.id]: { ...full, draft: full.draft.slice(0, 500), truncated: true } },
  }) as never)
  await until(() => byText('編輯後送出') !== undefined, '編輯後送出鈕')
  await act(async () => byText('編輯後送出')!.click())
  await until(() => textarea().value === LONG, '完整草稿放進輸入框')
  assert.equal(useStore.getState().composerDrafts[bot.id]?.editing, true)
  const edited = `${LONG} 改過`
  await act(async () => useStore.getState().setDraft(`bot:${bot.id}`, edited))
  await act(async () => document.querySelector<HTMLButtonElement>('.send-btn')!.click())
  await until(() => prompts(requests).length > 0, '送出 prompt')
  const body = prompts(requests)[0].body as Record<string, unknown>
  assert.equal(body.clear_draft, true)
  assert.equal(body.expect_draft_token, full.token, 'token 綁完整草稿，不是截短版')
  assert.equal(body.text, edited)
  assert.equal(body.submit_draft, undefined)
  assert.equal(body.queue_if_busy, undefined, '清掉再送不併 queue_if_busy')
})

test('Session paused 選第 2 項（Edit prompt and retry）：自動讀框並進入編輯；選第 1 項不動', { timeout: 30_000 }, async () => {
  resetStoreForTest()
  const menu = parseChoiceMenu(SESSION_PAUSED)
  assert.ok(menu)
  const calls: Array<{ botId: string; opts: unknown }> = []
  const original = { load: useStore.getState().loadComposerDraft, read: useStore.getState().readTerminal, keys: useStore.getState().sendKeys }
  await act(() =>
    useStore.setState({
      runs: { b1: { id: 'r1' } as Run },
      notices: [],
      readTerminal: async () => ({ text: SESSION_PAUSED }) as TerminalSnapshot,
      sendKeys: async () => {},
      loadComposerDraft: async (botId: string, opts: unknown) => {
        calls.push({ botId, opts })
        return null
      },
    } as never),
  )
  const root = await mount(<BlockedChoices botId="b1" menu={menu} />)
  const items = root.querySelectorAll<HTMLButtonElement>('.bc-item')
  await act(async () => items[0].click())
  await flush()
  assert.equal(calls.length, 0, 'Switch to … 不讀框')
  await act(async () => items[1].click())
  await until(() => calls.length === 1, '選了 Edit prompt and retry 之後讀框')
  assert.equal(calls[0].botId, 'b1')
  assert.equal((calls[0].opts as { editNow?: boolean }).editNow, true)
  // 這個測試換掉的 store 動作要還回去，不然後面的測試拿到的是假的。
  useStore.setState({ loadComposerDraft: original.load, readTerminal: original.read, sendKeys: original.keys } as never)
})

test('editNow：草稿一出現就自動放進輸入框（不必再按一次）', { timeout: 30_000 }, async () => {
  const { bot } = await openChat('am-claude')
  mock.composerDrafts.set(bot.id, '上一則 prompt')
  await act(async () => {
    await useStore.getState().loadComposerDraft(bot.id, { editNow: true })
  })
  await until(() => textarea().value === '上一則 prompt', '自動帶進輸入框')
  assert.equal(useStore.getState().composerDrafts[bot.id]?.editing, true)
  assert.equal(useStore.getState().composerDrafts[bot.id]?.editNow, false)
  mock.composerDrafts.set(bot.id, null)
})

test('composer_draft 事件：present:true 才去 GET 並掛上那一條；present:false 收掉；不輪詢', { timeout: 30_000 }, async () => {
  const { requests, bot } = await openChat('am-claude')
  const gets = () => requests.filter((r) => r.method === 'GET' && r.path.endsWith(`/bots/${bot.id}/composer-draft`))
  const before = gets().length
  // 事件沒來之前，網頁不會自己去讀框。
  mock.composerDrafts.set(bot.id, '終端那段')
  await flush()
  assert.equal(gets().length, before)
  assert.equal(useStore.getState().composerDrafts[bot.id], undefined)
  // 事件來了才讀，而且只讀一次。
  await act(async () => dispatchFrameForTest({ type: 'composer_draft', data: { bot_id: bot.id, run_id: 'r', present: true } }))
  await until(() => useStore.getState().composerDrafts[bot.id]?.draft === '終端那段', '掛上草稿列')
  assert.equal(gets().length, before + 1)
  await until(() => byText('編輯後送出') !== undefined, '三個動作出現')
  // 草稿不見了：收掉，輸入框裡使用者打的字不動。
  await act(async () => useStore.getState().setDraft(`bot:${bot.id}`, '我打的字'))
  await act(async () => dispatchFrameForTest({ type: 'composer_draft', data: { bot_id: bot.id, run_id: 'r', present: false } }))
  assert.equal(useStore.getState().composerDrafts[bot.id], undefined)
  assert.equal(useStore.getState().drafts[`bot:${bot.id}`], '我打的字')
  assert.equal(gets().length, before + 1, 'present:false 不再讀')
  mock.composerDrafts.set(bot.id, null)
})

test('選到 idle 的 bot：打一次 GET 補上錯過的草稿；每次選取一次、已有草稿列或不是 idle 就不打', { timeout: 30_000 }, async () => {
  const { requests, bot } = await openChat('am-claude')
  // 前面的測試送過 prompt：等 mock 把回合跑完回到 idle。
  await until(async () => {
    await useStore.getState().refreshState()
    return useStore.getState().runs[bot.id]?.agent_status === 'idle'
  }, 'bot 回到 idle')
  const gets = () => requests.filter((r) => r.method === 'GET' && r.path.endsWith(`/bots/${bot.id}/composer-draft`)).length
  const other = useStore.getState().bots.find((b) => b.id !== bot.id)!
  const reselect = async () => {
    await act(async () => useStore.getState().selectBot(other.id))
    await flush()
    await act(async () => useStore.getState().selectBot(bot.id))
    await flush()
  }
  mock.composerDrafts.set(bot.id, '錯過事件的草稿')
  const base = gets()
  await reselect()
  await until(() => useStore.getState().composerDrafts[bot.id]?.draft === '錯過事件的草稿', '選取時補上草稿列')
  assert.equal(gets(), base + 1, '每次選取一次')
  // 草稿列已經在：再選一次不重讀。
  await reselect()
  assert.equal(gets(), base + 1, '已有草稿列不再讀')
  // 沒有草稿：每次選取仍只讀一次，不掛。
  await act(async () => useStore.getState().dismissComposerDraft(bot.id))
  mock.composerDrafts.set(bot.id, null)
  await reselect()
  assert.equal(gets(), base + 2)
  assert.equal(useStore.getState().composerDrafts[bot.id], undefined)
  // 不是 idle（回合在跑）就不打。
  useStore.setState((s) => ({ runs: { ...s.runs, [bot.id]: { ...s.runs[bot.id], agent_status: 'working' } } }) as never)
  await reselect()
  assert.equal(gets(), base + 2, '不是 idle 不讀')
  useStore.setState((s) => ({ runs: { ...s.runs, [bot.id]: { ...s.runs[bot.id], agent_status: 'idle' } } }) as never)
})

test('選取補洞：連續呼叫只打一次（進行中不重複），沒有 run 或 pending bot 不打', { timeout: 30_000 }, async () => {
  const { requests, bot } = await openChat('am-claude')
  await until(async () => {
    await useStore.getState().refreshState()
    return useStore.getState().runs[bot.id]?.agent_status === 'idle'
  }, 'bot 回到 idle')
  const gets = () => requests.filter((r) => r.method === 'GET' && r.path.endsWith('/composer-draft')).length
  const base = gets()
  const { probeComposerDraftOnce } = await import('../store/store')
  await act(async () => {
    probeComposerDraftOnce(useStore.getState as never, bot.id)
    probeComposerDraftOnce(useStore.getState as never, bot.id)
    probeComposerDraftOnce(useStore.getState as never, 'pending:x')
  })
  await flush()
  assert.equal(gets(), base + 1)
})
