/**
 * 終端畫面解析出來的問卷（即時選單、多分頁預載）也走原題卡同一套譯文：英文自動翻、可切回原文，
 * 送出的鍵與原文一致（譯文只換顯示）；多分頁預載完後整份只打一次翻譯。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, fakeApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import type { Run, TerminalSnapshot } from '../api/types.ts'
import { parseChoiceMenu } from '../lib/tuiChoices.ts'
import { BlockedDraft } from './BlockedDraft.tsx'
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
  resetTranslateReady()
  resetPendingQuestionTranslation()
  resetStoreForTest()
})
after(teardownDom)

const SCREEN = [
  'Which logging level should the daemon use?',
  '',
  '❯ 1. Debug',
  '     Verbose output for every request',
  '  2. Info',
  '  3. Warn',
  '',
  'Enter to select · ↑/↓ to navigate · Esc to cancel',
].join('\n')
const SURVEY_FIRST = [
  '─'.repeat(60),
  '←  ☐ Editing  ☐ Features  ✔ Submit  →',
  '',
  'Who writes the articles?',
  '',
  '❯ 1. [ ] Non-engineers publish alone',
  '  2. [ ] Engineers edit Markdown',
  '',
  'Enter to select · Tab/Arrow keys to navigate · Esc to cancel',
].join('\n')
const SURVEY_SECOND = SURVEY_FIRST.replace('Who writes the articles?', 'Which features do you need besides reading?').replace(
  'Non-engineers publish alone',
  'Site search, categories, tags',
).replace('Engineers edit Markdown', 'Newsletter subscription')

const isTranslate = (p: string) => p.endsWith('/pending-questions/translate')
const SETTINGS = { enabled: true, key_present: true, key_error: null, models: [], efforts: [], uses: [{ id: 'message_translate', enabled: true, auto: true }] }

/** 假翻譯：每個欄位前面加「譯:」，好分辨顯示的是譯文。 */
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

const translateBodies = (reqs: { path: string; body?: unknown }[]) => reqs.filter((r) => isTranslate(r.path))

test('即時選單：英文自動翻成譯文；切回原文不再自動翻回去；送出的鍵都是原文', async () => {
  const sent: string[][] = []
  const reqs = fakeApi((r) => (r.path.endsWith('/claude-api/settings') ? SETTINGS : isTranslate(r.path) ? echo(r.body) : {}))
  const menu = parseChoiceMenu(SCREEN)
  assert.ok(menu)
  await act(() =>
    useStore.setState({
      runs: { b1: { id: 'r1' } as Run },
      notices: [],
      readTerminal: async () => ({ text: SCREEN }) as TerminalSnapshot,
      sendKeys: async (_b: string, keys: string[]) => {
        sent.push(keys)
      },
    } as never),
  )
  const root = await mount(<BlockedChoices botId="b1" menu={menu} />)
  await until(() => root.querySelector('.bc-title')?.textContent?.startsWith('譯:') === true, '自動翻出譯文')
  assert.equal(translateBodies(reqs).length, 1, '只打一次')
  const sentBody = translateBodies(reqs)[0]?.body as { questions: { options: { label: string }[] }[] }
  assert.equal(sentBody.questions[0]!.options[0]!.label, menu.choices[0]!.title, '送去翻的是原文')
  assert.equal(root.querySelectorAll('.bc-item').length, menu.choices.length, '選項數不變')
  assert.ok(root.querySelector('.bc-question')?.textContent?.startsWith('譯:'), '題目也是譯文')
  assert.equal(reqs.filter((r) => r.method === 'POST' && !isTranslate(r.path)).length, 0, '翻譯不送任何按鍵')

  await click(root.querySelector('.msg-translate') as HTMLButtonElement)
  assert.equal(root.querySelector('.bc-title')?.textContent, menu.choices[0]!.title, '切回原文')
  await new Promise((r) => setTimeout(r, 30))
  assert.equal(translateBodies(reqs).length, 1, '切回原文後不再自動翻回去')

  await act(async () => root.querySelector<HTMLButtonElement>('.bc-item')!.click())
  await new Promise((r) => setTimeout(r, 30))
  assert.ok(sent.length >= 1, '點選項有送鍵')
  assert.ok(sent.every((keys) => keys.every((k) => !k.includes('譯:'))), '送出的鍵沒有譯文')
})

test('多分頁問卷：預載完後整份只打一次翻譯，分頁與題目顯示譯文；送出的鍵是原文', async () => {
  let tab = 0
  const sent: string[][] = []
  const reqs = fakeApi((r) => (r.path.endsWith('/claude-api/settings') ? SETTINGS : isTranslate(r.path) ? echo(r.body) : {}))
  const menu = parseChoiceMenu(SURVEY_FIRST)
  assert.ok(menu)
  await act(() =>
    useStore.setState({
      runs: { b1: { id: 'r1' } as Run },
      notices: [],
      readTerminal: async () => ({ text: tab === 0 ? SURVEY_FIRST : SURVEY_SECOND }) as TerminalSnapshot,
      sendKeys: async (_b: string, keys: string[]) => {
        sent.push(keys)
        tab = tab === 0 ? 1 : 0
      },
    } as never),
  )
  const root = await mount(<BlockedDraft botId="b1" menu={menu} />)
  await until(() => root.querySelector<HTMLButtonElement>('.bc-submit') !== null, '問卷預載完成')
  await until(() => root.querySelector('.bc-page-q')?.textContent?.startsWith('譯:') === true, '譯文出現')
  await new Promise((r) => setTimeout(r, 30))
  const bodies = translateBodies(reqs)
  assert.equal(bodies.length, 1, '兩題一次呼叫')
  assert.equal((bodies[0]?.body as { questions: unknown[] }).questions.length, 2)
  assert.ok([...root.querySelectorAll('.bc-title')].every((e) => e.textContent?.startsWith('譯:')), '選項都是譯文')
  assert.ok([...root.querySelectorAll('.bc-tab')].some((e) => e.textContent?.includes('譯:')), '分頁名也是譯文')

  const before = sent.length
  await act(async () => root.querySelector<HTMLButtonElement>('.bc-item')!.click())
  await act(async () => root.querySelector<HTMLButtonElement>('.bc-submit')!.click())
  await until(() => sent.length > before, '送出有送鍵')
  assert.ok(sent.every((keys) => keys.every((k) => !k.includes('譯:'))), '送出的鍵沒有譯文')
  assert.equal(menu.tabs.filter((t) => !t.submit).length, 2)
})
