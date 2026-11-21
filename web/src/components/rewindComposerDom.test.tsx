import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import type { FakeRequest } from '../testing/domHarness'
import { flush } from '../testing/flush'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { ChatPanel } from './ChatPanel'

virtualMockTime()
afterEach(async () => {
  await unmountAll()
  const bot = useStore.getState().bots.find((b) => b.name === 'am-claude')
  if (bot) mock.composerDrafts.set(bot.id, null)
})
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const mock = sharedMock

async function openIdleChatWithAUserMessage(text: string) {
  const requests = mockApi(mock)
  await useStore.getState().refreshState()
  const bot = useStore.getState().bots.find((b) => b.name === 'am-claude')!
  if (useStore.getState().runs[bot.id]?.state !== 'running') {
    await mock.request('POST', `/bots/${bot.id}/start`)
    await until(async () => {
      await useStore.getState().refreshState()
      return useStore.getState().runs[bot.id]?.state === 'running'
    }, 'bot running')
  }
  await until(async () => {
    await useStore.getState().refreshState()
    return useStore.getState().runs[bot.id]?.agent_status === 'idle'
  }, 'bot idle before prompting')
  await mock.request('POST', `/bots/${bot.id}/prompt`, { text, client_request_id: `rw-${Date.now()}` })
  await until(async () => {
    await useStore.getState().refreshState()
    await useStore.getState().loadMessages(bot.id)
    return useStore.getState().runs[bot.id]?.agent_status === 'idle'
  }, 'bot idle')
  useStore.getState().selectBot(bot.id)
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  await flush()
  return { requests, bot }
}

const rewindButtonFor = (text: string) => {
  const bubble = [...document.querySelectorAll('article.msg.user')].find((a) => a.textContent?.includes(text))!
  return bubble.querySelector<HTMLButtonElement>('.msg-rewind')!
}
const dialogButton = (label: string) => [...document.querySelectorAll('[role=alertdialog] button')].find((b) => b.textContent === label)!
const rewinds = (requests: FakeRequest[]) => requests.filter((r) => r.method === 'POST' && /\/bots\/[^/]+\/rewind/.test(r.path))

test('#737 終端有字：確認框逐字列出，「清掉再倒回」帶回同一段字，完全相同才清掉並倒回', { timeout: 60_000 }, async () => {
  const { requests, bot } = await openIdleChatWithAUserMessage('rewind target one')
  mock.composerDrafts.set(bot.id, 'please  review this')
  await click(rewindButtonFor('rewind target one'))
  await click(dialogButton('倒回'))
  await until(() => document.querySelector('.rewind-draft') !== null, '終端輸入列有字的確認框')
  assert.equal(document.querySelector('.rewind-draft')!.textContent, 'please  review this', '兩個空白要原樣列出')
  assert.equal((rewinds(requests)[0].body as Record<string, unknown>).clear_composer, undefined, '第一次不帶 clear_composer')

  await click(dialogButton('清掉再倒回'))
  await until(() => rewinds(requests).length === 2, '第二次 rewind 請求')
  const second = rewinds(requests)[1].body as Record<string, unknown>
  assert.equal(second.clear_composer, true)
  assert.equal(second.expect_composer, 'please  review this', '逐字帶回 409 的那段，不能 trim 或壓空白')
  await until(() => Boolean(useStore.getState().messages[bot.id]?.find((m) => m.content === 'rewind target one')?.rewound_at), '那則被標成已倒回')
  assert.equal(mock.composerDrafts.has(bot.id), false, '終端輸入列清掉了')
})

test('#737 確認框出現後終端那段字只多了一個空白：不清、不倒回（composer_changed）', { timeout: 60_000 }, async () => {
  const { requests, bot } = await openIdleChatWithAUserMessage('rewind target two')
  mock.composerDrafts.set(bot.id, 'please review this')
  await click(rewindButtonFor('rewind target two'))
  await click(dialogButton('倒回'))
  await until(() => document.querySelector('.rewind-draft') !== null, '終端輸入列有字的確認框')

  mock.composerDrafts.set(bot.id, 'please  review this')
  await click(dialogButton('清掉再倒回'))
  await until(() => rewinds(requests).length === 2, '第二次 rewind 請求')
  await flush()
  assert.equal(mock.composerDrafts.has(bot.id), true, '新改的草稿不能被清掉')
  assert.equal(Boolean(useStore.getState().messages[bot.id]?.find((m) => m.content === 'rewind target two')?.rewound_at), false, '不能倒回')
  mock.composerDrafts.set(bot.id, null)
})
