import test from 'node:test'
import assert from 'node:assert/strict'
import type { TerminalSnapshot } from '../api/types'
import type { Io } from './choiceDraft.ts'
import {
  draftKey,
  forgetBlocked,
  peekPrefetched,
  prefetchBlocked,
  PREFETCH_FRESH_MS,
  PREFETCH_LINES,
  PREFETCH_SOURCE,
  resetBlockedPrefetch,
  type PrefetchDeps,
} from './blockedPrefetch.ts'

/** 真機那份（A 各種，2026-09-25）：一題複選＋送出頁。 */
const ONE_QUESTION = [
  '─'.repeat(60),
  '←  ☐ #478 守衛  ✔ Submit  →',
  '',
  '#478：要怎麼處理？',
  '',
  '❯ 1. [ ] 擋 DNS rebinding (Recommended)',
  '  2. [ ] shell類只限本機',
  '  3. [ ] Type something',
  '     Submit',
  '─'.repeat(60),
  '  4. Chat about this',
  '',
  'Enter to select · ↑/↓ to navigate · Esc to cancel',
].join('\n')

/** 兩題＋送出頁，第一題畫面。 */
const TWO_QUESTIONS = [
  '─'.repeat(60),
  '←  ☐ 編輯方式  ☐ 功能  ✔ Submit  →',
  '',
  '文章是誰寫、怎麼上稿？',
  '',
  '❯ 1. [ ] 非工程師要能自己發',
  '  2. [ ] 工程師改 Markdown',
  '  3. [ ] Type something',
  '     Submit',
  '',
  'Enter to select · Tab/Arrow keys to navigate · Esc to cancel',
].join('\n')

function snap(text: string): TerminalSnapshot {
  return { text } as TerminalSnapshot
}

function deps(text: string, { visible = true, now = 1_000 } = {}): PrefetchDeps & { sent: string[] } {
  const sent: string[] = []
  const io: Io = {
    read: async () => null,
    send: async (keys) => {
      sent.push(...keys)
    },
    wait: async () => {},
    paste: async () => {},
  }
  return { sent, read: async () => snap(text), io: () => io, visible: () => visible, now: () => now }
}

test('bot 一 blocked 就把快照存起來，視窗用同一組參數拿得到、過期就不拿', async () => {
  resetBlockedPrefetch()
  const d = deps(ONE_QUESTION)
  const menu = await prefetchBlocked('b1', d)
  assert.ok(menu, '認得出選單')
  assert.equal(peekPrefetched('b1', PREFETCH_SOURCE, PREFETCH_LINES, 1_000)?.text, ONE_QUESTION)
  assert.equal(peekPrefetched('b1', PREFETCH_SOURCE, 60, 1_000), null, '別組參數不拿快取')
  assert.equal(peekPrefetched('b1', PREFETCH_SOURCE, PREFETCH_LINES, 1_000 + PREFETCH_FRESH_MS + 1), null, '過期不拿')
  forgetBlocked('b1')
  assert.equal(peekPrefetched('b1', PREFETCH_SOURCE, PREFETCH_LINES, 1_000), null, '不再 blocked 就清掉')
})

test('單題問卷背景預載一顆鍵都不送', async () => {
  resetBlockedPrefetch()
  const d = deps(ONE_QUESTION)
  await prefetchBlocked('b1', d)
  await new Promise((r) => setTimeout(r, 0))
  assert.deepEqual(d.sent, [])
})

test('多分頁問卷在看得見的分頁才背景預載，而且同一份只起一次', async () => {
  resetBlockedPrefetch()
  const hidden = deps(TWO_QUESTIONS, { visible: false })
  await prefetchBlocked('b2', hidden)
  await new Promise((r) => setTimeout(r, 0))
  assert.deepEqual(hidden.sent, [], '背景分頁不送導覽鍵')

  const shown = deps(TWO_QUESTIONS)
  const menu = await prefetchBlocked('b2', shown)
  assert.ok(menu)
  assert.equal(draftKey('b2', menu), 'b2:編輯方式功能Submit')
  await new Promise((r) => setTimeout(r, 0))
  const first = shown.sent.length
  assert.ok(first > 0, '看得見就開始預載（送導覽鍵）')
  await prefetchBlocked('b2', shown)
  await new Promise((r) => setTimeout(r, 0))
  assert.equal(shown.sent.length, first, '同一份問卷不重起')
  resetBlockedPrefetch()
})
