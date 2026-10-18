import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { act, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import type { Run, TerminalSnapshot } from '../api/types.ts'
import { parseChoiceMenu } from '../lib/tuiChoices.ts'
import { BlockedDraft } from './BlockedDraft.tsx'
import { BlockedChoices } from './BlockedChoices.tsx'

const { resetStoreForTest, useStore } = await import('../store/store.ts')

before(setupDom)
afterEach(unmountAll)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const SCREEN = readFileSync(new URL('../lib/__fixtures__/claude-askq-preview-213x55.txt', import.meta.url), 'utf8')
const SURVEY_FIRST = [
  '─'.repeat(60),
  '←  ☐ 編輯方式  ☐ 功能  ✔ Submit  →',
  '',
  '文章是誰寫、怎麼上稿？',
  '',
  '❯ 1. [ ] 非工程師要能自己發',
  '  2. [ ] 工程師改 Markdown',
  '',
  'Enter to select · Tab/Arrow keys to navigate · Esc to cancel',
].join('\n')
const SURVEY_SECOND = SURVEY_FIRST.replace('文章是誰寫、怎麼上稿？', '除了看文章，還需要哪些功能？').replace(
  '非工程師要能自己發',
  '站內搜尋、分類、標籤',
).replace('工程師改 Markdown', '訂閱電子報')

test('選項按鍵失敗：保留 store 的單一通知，選單顯示提示且不產生 unhandled rejection', async () => {
  resetStoreForTest()
  const menu = parseChoiceMenu(SCREEN)
  assert.ok(menu)
  await act(() =>
    useStore.setState({
      runs: { b1: { id: 'r1' } as Run },
      notices: [],
      readTerminal: async () => ({ text: SCREEN }) as TerminalSnapshot,
      sendKeys: async () => {
        useStore.getState().notify('error', '送出按鍵失敗：temporary failure')
        throw new Error('temporary failure')
      },
    } as never),
  )
  const root = await mount(<BlockedChoices botId="b1" menu={menu} />)
  const unhandled: unknown[] = []
  const onUnhandled = (error: unknown) => unhandled.push(error)
  process.on('unhandledRejection', onUnhandled)
  try {
    const option = root.querySelector<HTMLButtonElement>('.bc-item')!
    await act(async () => option.click())
    await until(() => root.querySelector('[role="status"]')?.textContent?.includes('按鍵沒有送出去') === true, '按鍵錯誤提示出現')
    await new Promise<void>((resolve) => setImmediate(resolve))

    assert.equal(useStore.getState().notices.length, 1, '呼叫端沒有重複 notify')
    assert.deepEqual(unhandled, [])
  } finally {
    process.off('unhandledRejection', onUnhandled)
  }
})

test('BlockedDraft 的 sendKeys reject：送出錯誤留在問卷，不重複通知也不產生 unhandled rejection', async () => {
  resetStoreForTest()
  const menu = parseChoiceMenu(SURVEY_FIRST)
  assert.ok(menu)
  let tab = 0
  let sends = 0
  await act(() =>
    useStore.setState({
      runs: { b1: { id: 'r1' } as Run },
      notices: [],
      readTerminal: async () => ({ text: tab === 0 ? SURVEY_FIRST : SURVEY_SECOND }) as TerminalSnapshot,
      sendKeys: async (_botId: string, keys: string[]) => {
        if (sends++ < 2) {
          tab = keys[0] === 'right' ? 1 : 0
          return
        }
        useStore.getState().notify('error', '送出按鍵失敗：temporary failure')
        throw new Error('temporary failure')
      },
    } as never),
  )
  const root = await mount(<BlockedDraft botId="b1" menu={menu} />)
  const unhandled: unknown[] = []
  const onUnhandled = (error: unknown) => unhandled.push(error)
  process.on('unhandledRejection', onUnhandled)
  try {
    await until(() => root.querySelector<HTMLButtonElement>('.bc-submit') !== null, '問卷預載完成')
    assert.equal(sends, 2, '預載只送出左右導覽鍵')
    await act(() => root.querySelectorAll<HTMLButtonElement>('.bc-item')[0]!.click())
    await act(() => root.querySelector<HTMLButtonElement>('.bc-submit')!.click())
    await until(() => root.querySelector('[role="status"]')?.textContent?.includes('temporary failure') === true, '送出錯誤出現')
    await new Promise<void>((resolve) => setImmediate(resolve))

    assert.equal(useStore.getState().notices.length, 1, 'BlockedDraft 不重複發 store 已發的通知')
    assert.deepEqual(unhandled, [])
  } finally {
    process.off('unhandledRejection', onUnhandled)
  }
})
