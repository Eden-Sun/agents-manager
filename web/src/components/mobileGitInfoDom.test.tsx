/**
 * 手機（≤640px）「Git / 專案資訊 / 檔案」彈窗（真的掛 `ChatPanel` 進 happy-dom，後端是 `MockTransport`）：
 * - #929：彈窗裡畫「bot 給你的檔案」（手機沒有檔案暫存托盤，outbox 沒有別的入口）。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { act, click, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { ChatPanel } from './ChatPanel'

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const mock = sharedMock
const gitBtn = () => document.querySelector<HTMLButtonElement>('.mobile-git-info')!

/** 手機寬度：`matchMedia` 對 640px 斷點回 true。 */
function asPhone(): () => void {
  const original = window.matchMedia
  window.matchMedia = ((q: string) => ({ matches: q.includes('max-width: 640px'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  return () => (window.matchMedia = original)
}

async function running(name: string): Promise<string> {
  mockApi(mock)
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
  return bot.id
}

test('手機：Git 鈕打開的彈窗裡有「bot 給你的檔案」，列出 outbox 的檔名（#929）', async () => {
  const restore = asPhone()
  try {
    const id = await running('am-claude')
    await act(() => useStore.setState({ selectedBotId: id }))
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    assert.equal(document.querySelector('.outbox-files'), null, '彈窗沒開就沒有')
    assert.equal(gitBtn().getAttribute('aria-label'), 'Git / 專案資訊 / 檔案', '入口的名字要提到檔案')
    await click(gitBtn())
    await until(() => document.querySelector('.modal .outbox-files') !== null, '彈窗裡有 outbox 區塊')
    await until(() => (document.querySelector('.modal .outbox-files')?.textContent ?? '').includes('tracking.tsv'), '列出 mock 的檔名')
    assert.ok(document.querySelector('.modal')?.textContent?.includes('Git / 專案資訊 / 檔案'), '彈窗標題')
  } finally {
    restore()
  }
})

test('CSS：outbox 在彈窗裡不套托盤的高度上限', () => {
  const outbox = readFileSync(new URL('./outboxFiles.css', import.meta.url), 'utf8')
  assert.match(outbox, /\.modal \.outbox-files\s*\{\s*max-height:\s*none/)
})
