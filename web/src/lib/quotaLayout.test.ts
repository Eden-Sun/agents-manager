import test from 'node:test'
import assert from 'node:assert/strict'
import { QUOTA_FIT_START, nextQuotaFit, withFreshNeed } from './quotaLayout.ts'
import type { QuotaFitState } from './quotaLayout.ts'

function settle(s: QuotaFitState, avail: number, contentAt: (l: number) => number, hasHostTag: boolean): QuotaFitState {
  for (let i = 0; i < 10; i++) {
    const next = nextQuotaFit(s, avail, contentAt(s.level), hasHostTag)
    if (next === s) return s
    s = next
  }
  throw new Error('量測沒有收斂（兩個階段來回跳）')
}

const remote = (l: number) => (l === 0 ? 634 : l === 1 ? 590 : 420)

test('遠端五格放不下名牌：先收名牌，量表照舊完整', () => {
  assert.equal(settle(QUOTA_FIT_START, 612, remote, true).level, 1)
})

test('連名牌都收了還是放不下：收成單窗口，不疊字', () => {
  assert.equal(settle(QUOTA_FIT_START, 560, remote, true).level, 2)
})

test('寬度夠：完整量表＋名牌', () => {
  assert.equal(settle(QUOTA_FIT_START, 700, remote, true).level, 0)
})

test('本機沒有名牌：放不下就直接收合，不停在階段 1', () => {
  const local = (l: number) => (l === 2 ? 420 : 590)
  assert.equal(settle(QUOTA_FIT_START, 580, local, false).level, 2)
  assert.equal(settle(QUOTA_FIT_START, 600, local, false).level, 0)
})

test('收合後要寬回上次量到的內容寬才展開（遲滯，不來回跳）', () => {
  const collapsed = settle(QUOTA_FIT_START, 560, remote, true)
  assert.equal(collapsed.level, 2)
  assert.equal(settle(collapsed, 589, remote, true).level, 2)
  assert.equal(settle(collapsed, 590, remote, true).level, 1)
  assert.equal(settle(collapsed, 640, remote, true).level, 0)
})

test('展開時內容變寬（倒數字變長）放不下：退回收合並記下新的寬度', () => {
  const collapsed = settle(QUOTA_FIT_START, 560, remote, true)
  const grown = (l: number) => (l === 1 ? 610 : remote(l))
  const s = settle(collapsed, 600, grown, true)
  assert.equal(s.level, 2)
  assert.equal(s.need[1], 610)
})

test('差不到半像素不算放不下（rect 寬是小數）', () => {
  assert.equal(nextQuotaFit(QUOTA_FIT_START, 590, 590.4, false).level, 0)
})

test('左半縮到內容寬釋出的寬度，讓收合的額度列展開（#1269）', () => {
  const collapsed = settle(QUOTA_FIT_START, 560, remote, true)
  assert.equal(collapsed.level, 2)
  assert.equal(settle(collapsed, 632, remote, true).level, 1)
  assert.equal(settle(collapsed, 640, remote, true).level, 0)
})

test('收合時內容已變窄：用現在的完整寬更新需求，放得下就展開（#1777）', () => {
  const stale: QuotaFitState = { level: 2, need: [900, null] }
  assert.equal(nextQuotaFit(withFreshNeed(stale, 600), 760, 500, false).level, 0)
})

test('收合時現在的完整寬還是比可用寬大：維持收合（#1777）', () => {
  const stale: QuotaFitState = { level: 2, need: [900, null] }
  assert.equal(nextQuotaFit(withFreshNeed(stale, 800), 760, 500, false).level, 2)
})

test('完整量表階段不動需求寬（#1777）', () => {
  assert.equal(withFreshNeed(QUOTA_FIT_START, 600), QUOTA_FIT_START)
})
