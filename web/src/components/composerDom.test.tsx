/**
 * 對話輸入區的互動行為（真的掛 `ChatPanel` 進 happy-dom，後端是 `MockTransport`，前端走正式的 fetch／XHR 路徑）：
 * - IME 組字時 Enter 是「確認選字」，不能送出；組字結束的 Enter 才送。
 * - #733 回合中按 Enter＝請 daemon 排隊（`queue_if_busy`）：輸入框清空、出現「已排隊」；撤回把字與附件卡片放回輸入框；
 *   排隊的那一則落在 daemon，另一個分頁（全新的 store）讀回來看到同一則。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, imeEnter, keydown, mockApi, mount, settle, setupDom, teardownDom, typeInto, unmountAll, until } from '../testing/domHarness'
import type { FakeRequest } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { ChatPanel } from './ChatPanel'

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest() // 要在拆 DOM 之前：關 socket 會拿掉 window 上的監聽
  await teardownDom()
})

const textarea = () => document.querySelector<HTMLTextAreaElement>('.composer textarea')!
const prompts = (requests: FakeRequest[]) => requests.filter((r) => r.method === 'POST' && /\/bots\/[^/]+\/prompt/.test(r.path))
const queuedBar = () => document.querySelector('.composer-queued-idle')


// 整個檔案共用一個 mock 後端：store 會丟掉 `daemon_seq` 比較小的快照（防舊快照蓋新的），每個測試各建一個新 mock
// 等於「daemon 重啟」，store 就不更新了。不同測試用不同的 bot，免得上一個測試留下的回合／排隊互相干擾。
const mock = sharedMock

/** 起 `name` 這顆 bot 並等它 running，選取它、掛上 ChatPanel。 */
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
  await settle(200)
  return { requests, bot }
}

/** 讓 bot 進入回合中（`slow` 的回合 mock 會跑約 8 秒），並讓前端知道。 */
async function makeBusy(botId: string) {
  // 上一個測試留下的回合還在跑就不再送（mock 對回合中的 bot 會 409）。
  await mock.request('POST', `/bots/${botId}/prompt`, { text: 'slow job', client_request_id: `busy-${botId}-${Date.now()}` }).catch(() => {})
  await useStore.getState().loadMessages(botId)
  await settle(100)
}

test('IME 組字中的 Enter 不送出；組字結束後的 Enter 才送', { timeout: 30_000 }, async () => {
  const { requests } = await openChat('am-claude')
  await typeInto(textarea(), 'こんにちは')
  const composing = await imeEnter(textarea(), 'composing')
  assert.equal(composing.defaultPrevented, false, '組字中的 Enter 要留給輸入法')
  const webkit = await imeEnter(textarea(), 'keycode229')
  assert.equal(webkit.defaultPrevented, false, 'WebKit 在 compositionend 之後才送的 keyCode 229 也一樣')
  await settle(100)
  assert.equal(prompts(requests).length, 0, '沒送出任何 prompt')
  assert.equal(textarea().value, 'こんにちは', '字還在輸入框')

  const plain = await keydown(textarea(), 'Enter')
  assert.equal(plain.defaultPrevented, true)
  await settle(200)
  assert.equal(prompts(requests).length, 1)
  assert.equal((prompts(requests)[0].body as { text: string }).text, 'こんにちは')
})

test('#733 回合中按 Enter：請 daemon 排隊（queue_if_busy）、輸入框清空、出現「已排隊」；撤回放回輸入框', { timeout: 30_000 }, async () => {
  const { requests, bot } = await openChat('am-claude-2')
  await makeBusy(bot.id)
  await typeInto(textarea(), 'next question')
  await keydown(textarea(), 'Enter')
  await until(() => /next question/.test(queuedBar()?.textContent ?? ''), '已排隊的那一條（含 daemon 回讀的原文）')
  const body = prompts(requests).at(-1)!.body as { text: string; queue_if_busy?: boolean }
  assert.equal(body.text, 'next question')
  assert.equal(body.queue_if_busy, true, 'Enter 一律帶 queue_if_busy，是否排隊由 daemon 依忙碌與否決定')
  assert.match(queuedBar()!.textContent ?? '', /next question/)
  assert.equal(textarea().value, '', '送出後輸入框清空')

  const withdraw = [...queuedBar()!.querySelectorAll('button')].find((b) => b.textContent?.includes('撤回'))!
  await click(withdraw)
  await until(() => queuedBar() === null, '排隊條消失')
  assert.equal(textarea().value, 'next question', '撤回把字放回輸入框')
  assert.ok(requests.some((r) => r.method === 'POST' && /\/turns\/[^/]+\/withdraw/.test(r.path)), '真的打了 withdraw')
})

test('#733 撤回把附件卡片一起放回輸入框', { timeout: 30_000 }, async () => {
  const { requests, bot } = await openChat('am-codex')
  await makeBusy(bot.id)
  const input = document.querySelector<HTMLInputElement>('input[type=file]')!
  const file = new File([new Uint8Array([137, 80, 78, 71])], 'shot.png', { type: 'image/png' })
  Object.defineProperty(input, 'files', { value: [file], configurable: true })
  await act(async () => {
    input.dispatchEvent(new Event('change', { bubbles: true }))
  })
  await until(() => requests.some((r) => (r.body as { upload?: boolean } | undefined)?.upload) && document.querySelectorAll('.attach-thumb:not(.uploading)').length === 1, '附件上傳完（卡片不再是上傳中）')
  await typeInto(textarea(), 'see attached')
  await keydown(textarea(), 'Enter')
  await until(() => /附 1 個檔案/.test(queuedBar()?.textContent ?? ''), '已排隊（附 1 個檔案）')
  assert.equal(document.querySelectorAll('.attach-thumb').length, 0, '送出後附件卡片清掉')

  await click([...queuedBar()!.querySelectorAll('button')].find((b) => b.textContent?.includes('撤回'))!)
  await until(() => queuedBar() === null, '排隊條消失')
  assert.equal(textarea().value, 'see attached')
  await until(() => document.querySelectorAll('.attach-thumb').length === 1, '附件卡片放回輸入框')
})

test('#733 排隊的那一則落在 daemon：全新的 store（另一個分頁）讀回來看到同一則', { timeout: 30_000 }, async () => {
  const { bot } = await openChat('am-claude-2')
  await makeBusy(bot.id)
  await typeInto(textarea(), 'visible everywhere')
  await keydown(textarea(), 'Enter')
  await until(() => /visible everywhere/.test(queuedBar()?.textContent ?? ''), '已排隊')

  // 「另一個分頁」：本機的訊息、回合、載入記錄全清掉，只剩 daemon 那份。
  await unmountAll()
  useStore.setState({ messages: {}, turns: {}, loadedBots: {}, drafts: {} } as never)
  await useStore.getState().refreshState()
  useStore.getState().selectBot(bot.id)
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  await until(() => /visible everywhere/.test(queuedBar()?.textContent ?? ''), '另一個分頁也看到排隊中的那一則')
  assert.match(queuedBar()!.textContent ?? '', /visible everywhere/)
})
