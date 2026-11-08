/**
 * 終端 review 頁（Ready to submit…）的譯文：翻譯在飛的途中若父層換了（元件重掛），譯文仍要套上去；
 * auto 與手動翻譯都要顯示。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { useState } from 'react'
import { act, fakeApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import type { Run, TerminalSnapshot } from '../api/types.ts'
import { twoAskScreen } from '../api/mockTwoAsk'
import { parseChoiceMenu, type TuiChoiceMenu } from '../lib/tuiChoices.ts'
import { BlockedChoices } from './BlockedChoices.tsx'
import { resetTranslateReady } from './useTranslateReady'
import { resetPendingQuestionTranslation } from './usePendingQuestionTranslation'

const { resetStoreForTest, useStore } = await import('../store/store.ts')

before(setupDom)
beforeEach(() => {
  resetTranslateReady()
  resetPendingQuestionTranslation()
})
afterEach(async () => {
  await unmountAll()
  resetStoreForTest()
  // 模組層級的快取（設定、譯文）不能帶到同一個 bun 程序的下一個檔案。
  resetTranslateReady()
  resetPendingQuestionTranslation()
})
after(teardownDom)

const REVIEW = twoAskScreen({ stage: 2, cursor: 0, picked: 0, checked: [false, true] }).join('\n')
const isTranslate = (p: string) => p.endsWith('/pending-questions/translate')
const SETTINGS = (auto: boolean) => ({ enabled: true, key_present: true, key_error: null, models: [], efforts: [], uses: [{ id: 'message_translate', enabled: true, auto }] })

function echo(body: unknown) {
  const qs = (body as { questions: { header: string; question: string; options: { label: string; description: string }[] }[] }).questions
  return {
    questions: qs.map((q, qi) => ({
      id: `q${qi}`,
      header: `譯:${q.header}`,
      question: `譯:${q.question}`,
      options: q.options.map((o, oi) => ({ id: `q${qi}-o${oi}`, label: `譯:${o.label}`, description: o.description ? `譯:${o.description}` : '' })),
    })),
    model: 'claude-haiku-5-5',
    cached: false,
    cost_usd: null,
  }
}

const setStore = () =>
  act(() =>
    useStore.setState({
      runs: { b1: { id: 'r1' } as Run },
      notices: [],
      readTerminal: async () => ({ text: REVIEW }) as TerminalSnapshot,
      sendKeys: async () => {},
    } as never),
  )

test('review 頁：auto 開著就自動翻，題目顯示譯文', async () => {
  const menu = parseChoiceMenu(REVIEW) as TuiChoiceMenu
  const reqs = fakeApi((r) => (r.path.endsWith('/claude-api/settings') ? SETTINGS(true) : isTranslate(r.path) ? echo(r.body) : {}))
  await setStore()
  const root = await mount(<BlockedChoices botId="b1" menu={menu} />)
  await until(() => root.querySelector('.bc-question')?.textContent?.startsWith('譯:') === true, '自動翻出譯文')
  assert.equal(reqs.filter((r) => isTranslate(r.path)).length, 1)
})

test('review 頁：翻譯在飛的途中父層換了（元件重掛），譯文仍要顯示，且不重打', async () => {
  const menu = parseChoiceMenu(REVIEW) as TuiChoiceMenu
  const reqs = fakeApi((r) => (r.path.endsWith('/claude-api/settings') ? SETTINGS(true) : isTranslate(r.path) ? echo(r.body) : {}))
  await setStore()
  let flip: (v: boolean) => void = () => {}
  function Harness() {
    const [wrapped, setWrapped] = useState(false)
    flip = setWrapped
    const choices = <BlockedChoices botId="b1" menu={menu} />
    return wrapped ? <div>{choices}</div> : choices
  }
  const root = await mount(<Harness />)
  await until(() => reqs.some((r) => isTranslate(r.path)), '送出翻譯')
  await act(async () => flip(true))
  await until(() => root.querySelector('.bc-question')?.textContent?.startsWith('譯:') === true, '重掛後仍顯示譯文')
  assert.equal(reqs.filter((r) => isTranslate(r.path)).length, 1, '不重打')
})

test('auto 關著：手動按翻譯後，review 頁顯示譯文', async () => {
  const menu = parseChoiceMenu(REVIEW) as TuiChoiceMenu
  const reqs = fakeApi((r) => (r.path.endsWith('/claude-api/settings') ? SETTINGS(false) : isTranslate(r.path) ? echo(r.body) : {}))
  await setStore()
  const root = await mount(<BlockedChoices botId="b1" menu={menu} />)
  await until(() => root.querySelector('.msg-translate') !== null, '翻譯鈕')
  await act(async () => (root.querySelector('.msg-translate') as HTMLButtonElement).click())
  await until(() => root.querySelector('.bc-question')?.textContent?.startsWith('譯:') === true, '手動後顯示譯文')
  assert.equal(reqs.filter((r) => isTranslate(r.path)).length, 1)
})
