import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { BlockedChoices } from './BlockedChoices.tsx'
import { parseChoiceMenu } from '../lib/tuiChoices.ts'
import type { PendingAt } from '../lib/pendingQuestion.ts'

/** 分頁列上標「目前分頁」的那顆（`bc-tab-at`／`aria-current`）的文字。 */
function currentTab(html: string): string | null {
  const m = /<button[^>]*class="bc-tab bc-tab-at[^"]*"[^>]*>(.*?)<\/button>/.exec(html)
  if (!m) return null
  return m[1].replace(/<[^>]+>/g, '').replace(/^[☐☑✔]\s*/, '').trim()
}

const SCREEN_Q1 = [
  '←  ☒ Scope  ☒ Style  ✔ Submit  →',
  '',
  'Which scope should this apply to?',
  '❯ 1. Project',
  '  2. Global',
  '',
  'Enter to select · ↑/↓ to navigate · Esc to cancel',
].join('\n')

const PENDING_Q1: PendingAt = {
  at: 0,
  total: 2,
  question: 'Which scope should this apply to?',
  header: 'Scope',
  jev: null,
}

test('全部題都 ☒、畫面是 Q1：分頁列不標到「提交」，改標 Q1（與畫面一致）', () => {
  const menu = parseChoiceMenu(SCREEN_Q1)
  assert.ok(menu)
  assert.equal(menu.tabs.length, 3)
  const html = renderToStaticMarkup(<BlockedChoices botId="b1" menu={menu} pendingAt={PENDING_Q1} />)
  assert.equal(currentTab(html), 'Scope')
})

test('全部題都 ☒、畫面是 Q2（原題對得上第二題）：標 Q2', () => {
  const screen = SCREEN_Q1.replace('Which scope should this apply to?', 'Which style do you prefer?').replace('❯ 1. Project', '❯ 1. Plain').replace('2. Global', '2. Fancy')
  const menu = parseChoiceMenu(screen)
  assert.ok(menu)
  const pendingAt: PendingAt = { ...PENDING_Q1, at: 1, question: 'Which style do you prefer?', header: 'Style' }
  const html = renderToStaticMarkup(<BlockedChoices botId="b1" menu={menu} pendingAt={pendingAt} />)
  assert.equal(currentTab(html), 'Style')
})

test('畫面是送出頁（Review）：標「提交」是對的', () => {
  const review = [
    '←  ☒ Scope  ☒ Style  ✔ Submit  →',
    '',
    'Review your answers',
    ' ● Which scope should this apply to? → Project',
    ' ● Which style do you prefer? → Plain',
    '',
    'Ready to submit your answers?',
    '❯ 1. Submit answers',
    '  2. Cancel',
    '',
    'Enter to select · ↑/↓ to navigate · Esc to cancel',
  ].join('\n')
  const menu = parseChoiceMenu(review)
  assert.ok(menu)
  const html = renderToStaticMarkup(<BlockedChoices botId="b1" menu={menu} />)
  assert.equal(currentTab(html), 'Submit')
})
