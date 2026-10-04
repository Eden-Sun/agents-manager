/**
 * claude 的「建議下一句」那一條（真的掛 `ChatPanel` 進 happy-dom，後端是 `MockTransport`）：
 * 輸入框空的、claude 閒著才顯示；打字就收起；點一下送 `POST /suggestion/accept`，之後開一個回合、那一條消失。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mockApi, mount, settle, setupDom, teardownDom, typeInto, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { ChatPanel } from './ChatPanel'

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})

const mock = sharedMock
const bar = () => document.querySelector<HTMLElement>('.suggestion-bar')
const button = () => document.querySelector<HTMLButtonElement>('.suggestion-bar-btn')
const textarea = () => document.querySelector<HTMLTextAreaElement>('.composer textarea')!

const SUGGESTION = '跑一次完整測試，確認都綠再收尾'

/** 整個行程共用一個 mock：別的測試檔可能已經讓這顆 bot 跑過回合，所以等它閒下來再明確放一句建議。 */
async function openChat(name: string, suggestion: string | null = null) {
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
  if (suggestion) {
    await until(async () => {
      await useStore.getState().refreshState()
      return useStore.getState().runs[bot.id]?.agent_status === 'idle'
    }, `${name} idle`)
    mock.setPromptSuggestion(bot.id, suggestion)
    await useStore.getState().refreshState()
  }
  useStore.getState().selectBot(bot.id)
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  await settle(200)
  return { requests, bot }
}

test('閒著的 claude 有建議：輸入列上方一行，顯示那句；打字就收起，清掉又回來', async () => {
  await openChat('am-claude', SUGGESTION)
  assert.ok(bar(), '有那一條')
  assert.match(button()!.textContent ?? '', new RegExp(SUGGESTION))
  assert.match(button()!.textContent ?? '', /Tab/, '標明等於終端的 Tab')
  assert.equal(button()!.disabled, false)

  await typeInto(textarea(), '我自己要打的')
  assert.equal(bar(), null, '輸入框有字就不顯示')
  await typeInto(textarea(), '')
  assert.ok(bar(), '清空又回來')
})

test('點一下：送 accept（帶那句與 run id），開回合、那一條消失、對話窗出現那一句', async () => {
  const { requests, bot } = await openChat('am-claude', SUGGESTION)
  await click(button()!)
  await settle(200)
  const accepts = requests.filter((r) => r.method === 'POST' && /\/bots\/[^/]+\/suggestion\/accept/.test(r.path))
  assert.equal(accepts.length, 1)
  const body = accepts[0].body as Record<string, unknown>
  assert.equal(body.suggestion, SUGGESTION)
  assert.equal(body.expect_run_id, useStore.getState().runs[bot.id]?.id)
  await until(() => bar() === null, '那一條消失（回合開始、不再 idle）')
  await useStore.getState().loadMessages(bot.id)
  const user = (useStore.getState().messages[bot.id] ?? []).filter((m) => m.role === 'user').at(-1)
  assert.equal(user?.content, SUGGESTION)
  // 共用的 mock：讓這一回合收尾、bot 回到閒著，別拖累後面用同一顆 bot 的測試檔。
  await until(async () => {
    await useStore.getState().refreshState()
    return useStore.getState().runs[bot.id]?.agent_status === 'idle'
  }, '回合收尾')
  await act(async () => {})
})

test('不是閒著、不是 claude 的 bot：不顯示', async () => {
  await openChat('am-claude-2')
  assert.equal(bar(), null, '沒有建議的 bot 沒有那一條')
})

test('CLI 建議文字以純文字顯示，HTML 標籤不會變成 DOM', async () => {
  const payload = '<img src=x onerror=alert(1)> & <svg onload=alert(2)>'
  await openChat('am-claude', payload)
  const el = button()!
  assert.equal(el.querySelector('img, svg'), null, 'CLI 內容不得建立可執行元素')
  assert.ok(el.textContent?.includes(payload), '原字串照樣可見')
  assert.ok(el.title.startsWith(payload), 'title 也保持為字串內容')
})
