/**
 * 終端停在多分頁問卷的 Submit（Review）分頁：從那裡預載（不退回即時模式）、停在第一個未答題、
 * 英文自動翻成譯文，而且預載只送導覽鍵。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, fakeApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import type { Run, TerminalSnapshot } from '../api/types.ts'
import { parseChoiceMenu } from '../lib/tuiChoices.ts'
import { BlockedDraft } from './BlockedDraft.tsx'
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
  resetTranslateReady()
  resetPendingQuestionTranslation()
})
after(teardownDom)

const PAGES = [
  { label: 'Who', q: 'Who writes the articles?', opts: ['Non-engineers publish alone', 'Engineers edit Markdown'], answer: 'Engineers edit Markdown' },
  { label: 'Features', q: 'Which features do you need?', opts: ['Site search, categories, tags', 'Newsletter subscription'], answer: 'Newsletter subscription' },
]

/** 假終端：分頁列、每頁的題目、Review 頁。導覽鍵只動 `tab`；其他鍵不改狀態。 */
class FakeEnglishTui {
  tab = 0
  sent: string[] = []
  bar(): string {
    const cells = PAGES.map((p) => `${process.env.ALLDONE ? '☒' : '☐'} ${p.label}`)
    cells.push('✔ Submit')
    return `←  ${cells.join('  ')}  →`
  }
  screen(): string {
    const out = ['─'.repeat(60), this.bar(), '']
    if (this.tab === 2) {
      out.push('Review your answers', '')
      for (const p of PAGES) out.push(` ● ${p.q}`, `   → ${p.answer}`)
      out.push('', 'Ready to submit your answers?', '', '❯ 1. Submit answers', '  2. Cancel', '', 'Enter to select · Esc to cancel')
      return out.join('\n')
    }
    const p = PAGES[this.tab]
    out.push(p.q, '')
    p.opts.forEach((o, i) => out.push(`${i === 0 ? '❯ ' : '  '}${i + 1}. ${o}`))
    out.push('', 'Enter to select · Tab/Arrow keys to navigate · Esc to cancel')
    return out.join('\n')
  }
  io(): void {}
}

const SETTINGS = { enabled: true, key_present: true, key_error: null, models: [], efforts: [], uses: [{ id: 'message_translate', enabled: true, auto: true }] }
const isTranslate = (p: string) => p.endsWith('/pending-questions/translate')
function echo(body: unknown) {
  const qs = (body as { questions: { header: string; question: string; options: { label: string; description: string }[] }[] }).questions
  return {
    questions: qs.map((q, qi) => ({ id: `q${qi}`, header: `譯:${q.header}`, question: `譯:${q.question}`, options: q.options.map((o, oi) => ({ id: `q${qi}-o${oi}`, label: `譯:${o.label}`, description: '' })) })),
    model: 'claude-haiku-5-5',
    cached: false,
    cost_usd: null,
  }
}

test('停在 Submit 分頁：預載不退回即時模式，英文自動翻（整份一次），預載只送導覽鍵', async () => {
  const tui = new FakeEnglishTui()
  tui.tab = 2
  const reqs = fakeApi((r) => (r.path.endsWith('/claude-api/settings') ? SETTINGS : isTranslate(r.path) ? echo(r.body) : {}))
  await act(() =>
    useStore.setState({
      runs: { b1: { id: 'r1' } as Run },
      notices: [],
      readTerminal: async () => ({ text: tui.screen() }) as TerminalSnapshot,
      sendKeys: async (_b: string, keys: string[]) => {
        for (const k of keys) {
          tui.sent.push(k)
          if (k === 'left' || k === 'shift+tab') tui.tab = Math.max(0, tui.tab - 1)
          if (k === 'right' || k === 'tab') tui.tab = Math.min(2, tui.tab + 1)
        }
      },
    } as never),
  )
  const menu = parseChoiceMenu(tui.screen())
  assert.ok(menu)
  assert.equal(menu.tabAt, 2, '畫面在 Submit，tabAt 也要是 Submit')
  const root = await mount(<BlockedDraft botId="b1" menu={menu} />)
  await until(() => root.querySelector('.bc-submit') !== null, '預載完成（不退回即時模式）')
  await until(() => root.querySelector('.bc-title')?.textContent?.startsWith('譯:') === true, '自動翻出譯文')
  await new Promise((r) => setTimeout(r, 30))
  assert.equal(reqs.filter((r) => isTranslate(r.path)).length, 1, '整份只打一次')
  assert.equal((reqs.find((r) => isTranslate(r.path))?.body as { questions: unknown[] }).questions.length, 2)
  assert.ok(tui.sent.length > 0 && tui.sent.every((k) => ['left', 'right', 'tab', 'shift+tab'].includes(k)), '只送導覽鍵')
  assert.equal(tui.tab, 0, '預載停在第一個未答題')
  // 已答的題（Review 上的答案）預設帶入：第二題選了 Newsletter subscription
  const picked = [...root.querySelectorAll('.bc-page')].map((page) => page.querySelector('.bc-item[aria-checked="true"]')?.textContent ?? '')
  assert.match(picked[1] ?? '', /Newsletter/, '終端上已答的那一項預設選中')
  assert.match(picked[0] ?? '', /Engineers/, '第一題也答過，帶入')
})
