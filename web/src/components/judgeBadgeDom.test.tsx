/**
 * bot 上的 Jev 徽章（issue #1328）：真的掛 `JudgeBadge` 進 happy-dom。
 * 超過門檻才出現；點開看說明；關掉之後不再出現；Jev 沒開／帳本 404 完全不顯示；帳本重抓最短 60 秒一次。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, settle, teardownDom, unmountAll } from '../testing/domHarness'
import { resetJudgeLedgerForTest, useJudgeLedger } from '../store/judgeLedger'
import { DISMISSED_KEY } from '../lib/judgeBadge'
import { JudgeBadge } from './JudgeBadge'

afterEach(unmountAll)
before(setupDom)
after(() => {
  resetJudgeLedgerForTest()
  teardownDom()
})

const ago = (h: number) => new Date(Date.now() - h * 3600_000).toISOString()
const row = (p: Record<string, unknown>) => ({
  bot_id: 'bot-a',
  run_id: null,
  kind: null,
  matched_line: null,
  composer_idle: null,
  jev_is_live_ui: null,
  jev_same_work: null,
  model: null,
  ms: null,
  input_tokens: null,
  error: null,
  cleared_at: null,
  assignment_id: null,
  claims_verified: null,
  asks_parent_action: null,
  ...p,
})

/** 本地 fetch 替身：路徑（不含 query）→ 狀態碼與內容；`hits` 記下每個路徑被打了幾次。 */
function stubApi(routes: Record<string, { status: number; body?: unknown }>) {
  const hits: Record<string, number> = {}
  globalThis.fetch = (async (input: string) => {
    const path = String(input).replace(/^\/api/, '')
    if (path === '/session') return new Response(JSON.stringify({ token: 'test-token' }), { status: 200 })
    const key = path.split('?')[0]!
    hits[key] = (hits[key] ?? 0) + 1
    const r = routes[key] ?? { status: 404, body: {} }
    return new Response(JSON.stringify(r.body ?? {}), { status: r.status })
  }) as unknown as typeof fetch
  return hits
}

function shadowWith(rows: unknown[], enabled = true) {
  return { status: 200, body: { enabled, projects: [], model: 'jev-1.13.0', rows } }
}

function seed(rows: unknown[], visible = true) {
  resetJudgeLedgerForTest()
  // 真實的 app 只有成功重抓後才會 visible，那時 lastFetchAt 一定有值（「近 24 小時」以它為準）。
  useJudgeLedger.setState({ rows: rows as never, visible, dismissed: new Set(), lastFetchAt: Date.now() })
}

test('超過門檻才出現徽章；title 寫出場景與機率', async () => {
  seed([row({ id: 'hit', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.9 })])
  const root = await mount(<JudgeBadge botId="bot-a" />)
  const btn = root.querySelector<HTMLButtonElement>('.judge-badge')
  assert.ok(btn, '超過門檻要有徽章')
  assert.match(btn.title, /卡在不認識的畫面/)
  assert.match(btn.title, /0\.90/)
})

test('低於門檻、別的 bot、超過 24 小時：都不出現', async () => {
  seed([
    row({ id: 'low', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.2 }),
    row({ id: 'other', regex_verdict: 'stuck_queued', at: ago(1), bot_id: 'bot-b', jev_is_live_ui: 0.99 }),
    row({ id: 'old', regex_verdict: 'stuck_queued', at: ago(48), jev_is_live_ui: 0.99 }),
  ])
  const root = await mount(<JudgeBadge botId="bot-a" />)
  assert.equal(root.querySelector('.judge-badge'), null)
})

test('點開：寫明場景、機率、遮罩後的命中行，以及「不會自動動作」', async () => {
  seed([row({ id: 'hit', regex_verdict: 'same_work', at: ago(1), jev_same_work: 0.6, matched_line: 'task A ↔ task B' })])
  const root = await mount(<JudgeBadge botId="bot-a" />)
  await click(root.querySelector('.judge-badge')!)
  await settle(20)
  const pop = root.querySelector('.judge-badge-pop')!
  assert.ok(pop, '點了要開浮層')
  assert.match(pop.textContent ?? '', /撞題提示/)
  assert.match(pop.textContent ?? '', /0\.60/)
  assert.match(pop.textContent ?? '', /task A ↔ task B/)
  assert.match(pop.textContent ?? '', /不會自動動作/)
})

test('關掉這則：立刻不見，並記在 localStorage（重開也不再出現）', async () => {
  seed([row({ id: 'gone-1', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.9 })])
  const root = await mount(<JudgeBadge botId="bot-a" />)
  await click(root.querySelector('.judge-badge')!)
  await settle(20)
  const dismiss = [...root.querySelectorAll('button')].find((b) => b.textContent?.includes('關掉這則'))!
  await click(dismiss)
  await settle(20)
  assert.equal(root.querySelector('.judge-badge'), null, '關掉之後不顯示')
  assert.ok(useJudgeLedger.getState().dismissed.has('gone-1'))
  assert.match(localStorage.getItem(DISMISSED_KEY) ?? '', /gone-1/)
})

test('Jev 沒開：帳本回 enabled=false，徽章完全不顯示', async () => {
  stubApi({ '/judge/shadow': shadowWith([row({ id: 'x', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.9 })], false) })
  resetJudgeLedgerForTest()
  await useJudgeLedger.getState().refresh({ force: true })
  const root = await mount(<JudgeBadge botId="bot-a" />)
  assert.equal(root.querySelector('.judge-badge'), null)
})

test('帳本 404（舊 daemon）：不顯示、不報錯', async () => {
  stubApi({})
  resetJudgeLedgerForTest()
  await useJudgeLedger.getState().refresh({ force: true })
  const root = await mount(<JudgeBadge botId="bot-a" />)
  assert.equal(root.querySelector('.judge-badge'), null)
  assert.equal(root.textContent?.trim(), '', '不畫任何文字')
})

test('帳本重抓最短 60 秒一次：連續要求只打一次；強制重抓才再打', async () => {
  const hits = stubApi({ '/judge/shadow': shadowWith([]) })
  resetJudgeLedgerForTest()
  await act(async () => {
    await useJudgeLedger.getState().refresh()
    await useJudgeLedger.getState().refresh()
  })
  assert.equal(hits['/judge/shadow'], 1, '60 秒內第二次不打')
  await act(async () => {
    await useJudgeLedger.getState().refresh({ force: true })
  })
  assert.equal(hits['/judge/shadow'], 2, '強制重抓要打')
})

/** 每則測試開頭還原：帳本 store 與關掉紀錄（localStorage）都不跟別則共用。 */
function freshBadgeState() {
  localStorage.removeItem(DISMISSED_KEY)
}

const pressKey = (key: string, init: KeyboardEventInit = {}) =>
  act(async () => {
    document.dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true, ...init }))
  })

const pressMouseDown = (target: Element) =>
  act(async () => {
    target.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }))
  })

async function openBadge(root: HTMLElement) {
  await click(root.querySelector('.judge-badge')!)
  await settle(20)
  return root.querySelector<HTMLElement>('.judge-badge-pop')
}

test('打開後焦點進浮層', async () => {
  freshBadgeState()
  seed([row({ id: 'focus-1', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.9 })])
  const root = await mount(<JudgeBadge botId="bot-a" />)
  const pop = await openBadge(root)
  assert.ok(pop, '點了要開浮層')
  assert.ok(pop.contains(document.activeElement), '焦點要在浮層裡，讀屏才知道多了一個 dialog')
})

test('Esc 關掉浮層、焦點回到徽章鈕', async () => {
  freshBadgeState()
  seed([row({ id: 'esc-1', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.9 })])
  const root = await mount(<JudgeBadge botId="bot-a" />)
  assert.ok(await openBadge(root))
  await pressKey('Escape')
  await settle(20)
  assert.equal(root.querySelector('.judge-badge-pop'), null, 'Esc 要關掉浮層')
  assert.equal(document.activeElement, root.querySelector('.judge-badge'), '焦點回到徽章鈕，不掉到 body')
  assert.equal(useJudgeLedger.getState().dismissed.has('esc-1'), false, 'Esc 只關浮層，不是把這一則永久收掉')
})

test('點浮層外面關掉、點浮層裡面不關', async () => {
  freshBadgeState()
  seed([row({ id: 'out-1', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.9 })])
  const root = await mount(<JudgeBadge botId="bot-a" />)
  const pop = await openBadge(root)
  assert.ok(pop)
  await pressMouseDown(pop.querySelector('p')!)
  await settle(20)
  assert.ok(root.querySelector('.judge-badge-pop'), '點浮層裡面不關')
  await pressMouseDown(document.body)
  await settle(20)
  assert.equal(root.querySelector('.judge-badge-pop'), null, '點外面要關掉')
})

test('輸入法組字中的 Esc 不關', async () => {
  freshBadgeState()
  seed([row({ id: 'ime-1', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.9 })])
  const root = await mount(<JudgeBadge botId="bot-a" />)
  assert.ok(await openBadge(root))
  await pressKey('Escape', { isComposing: true })
  await settle(20)
  assert.ok(root.querySelector('.judge-badge-pop'), '組字中的 Esc 是給輸入法的，不能關浮層')
})

test('關掉這則後還有次新的一則：焦點停在徽章鈕上', async () => {
  freshBadgeState()
  seed([
    row({ id: 'next-a', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.9 }),
    row({ id: 'next-b', regex_verdict: 'stuck_queued', at: ago(2), jev_is_live_ui: 0.9 }),
  ])
  const root = await mount(<JudgeBadge botId="bot-a" />)
  const pop = await openBadge(root)
  assert.ok(pop)
  const dismiss = [...pop.querySelectorAll('button')].find((b) => b.textContent?.includes('關掉這則'))!
  await click(dismiss)
  await settle(20)
  assert.ok(root.querySelector('.judge-badge'), '次新的那一則接上，徽章還在')
  assert.equal(document.activeElement, root.querySelector('.judge-badge'), '焦點回到徽章鈕（不是 body）')
})

test('關掉這則後沒有別的一則：焦點交給最近的可聚焦祖先（列），不掉到 body', async () => {
  freshBadgeState()
  seed([row({ id: 'last-1', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.9 })])
  const host = await mount(
    <div tabIndex={0} data-testid="row-host">
      <JudgeBadge botId="bot-a" />
    </div>,
  )
  const pop = await openBadge(host)
  assert.ok(pop)
  const dismiss = [...pop.querySelectorAll('button')].find((b) => b.textContent?.includes('關掉這則'))!
  await click(dismiss)
  await settle(20)
  const listRow = host.querySelector<HTMLElement>('[data-testid="row-host"]')!
  assert.equal(host.querySelector('.judge-badge'), null, '最後一則關掉，徽章整個不見')
  assert.equal(document.activeElement, listRow, '焦點交給列（tabIndex 祖先），不掉到 body')
})

test('浮層是 fixed 定位，並帶算出來的 left／top（不被側欄捲動區裁掉）', async () => {
  freshBadgeState()
  seed([row({ id: 'fixed-1', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.9 })])
  const root = await mount(<JudgeBadge botId="bot-a" />)
  const pop = await openBadge(root)
  assert.ok(pop)
  assert.equal(pop.style.position, 'fixed')
  assert.notEqual(pop.style.left, '')
  assert.notEqual(pop.style.top, '')
})

test('捲動或改視窗大小時關掉浮層（fixed 的浮層不跟著列走，留著會指錯列）', async () => {
  freshBadgeState()
  seed([row({ id: 'scroll-1', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.9 })])
  const root = await mount(<JudgeBadge botId="bot-a" />)
  assert.ok(await openBadge(root))
  await act(async () => {
    window.dispatchEvent(new Event('scroll'))
  })
  await settle(20)
  assert.equal(root.querySelector('.judge-badge-pop'), null, '捲動要關掉')
  assert.ok(await openBadge(root))
  await act(async () => {
    window.dispatchEvent(new Event('resize'))
  })
  await settle(20)
  assert.equal(root.querySelector('.judge-badge-pop'), null, '改視窗大小要關掉')
})

test('捲浮層裡的命中行不關浮層（長命中行要捲得完）', async () => {
  freshBadgeState()
  seed([row({ id: 'long-1', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.9, matched_line: 'x\n'.repeat(50) })])
  const root = await mount(<JudgeBadge botId="bot-a" />)
  const pop = await openBadge(root)
  assert.ok(pop)
  await act(async () => {
    pop.querySelector('.judge-badge-line')!.dispatchEvent(new Event('scroll'))
  })
  await settle(20)
  assert.ok(root.querySelector('.judge-badge-pop'), '浮層自己的捲動不關')
})

test('不相干區域的捲動不關浮層（對話區自動貼底不能把浮層關掉）', async () => {
  freshBadgeState()
  seed([row({ id: 'other-1', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.9 })])
  const root = await mount(
    <>
      <div data-testid="other" />
      <JudgeBadge botId="bot-a" />
    </>,
  )
  assert.ok(await openBadge(root))
  await act(async () => {
    root.querySelector('[data-testid="other"]')!.dispatchEvent(new Event('scroll'))
  })
  await settle(20)
  assert.ok(root.querySelector('.judge-badge-pop'), '別的區域捲動不關')
})

test('徽章所在的捲動容器捲動才關浮層（位置會錯）', async () => {
  freshBadgeState()
  seed([row({ id: 'scroller-1', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.9 })])
  const root = await mount(
    <div data-testid="scroller">
      <JudgeBadge botId="bot-a" />
    </div>,
  )
  assert.ok(await openBadge(root))
  await act(async () => {
    root.querySelector('[data-testid="scroller"]')!.dispatchEvent(new Event('scroll'))
  })
  await settle(20)
  assert.equal(root.querySelector('.judge-badge-pop'), null, '徽章所在的容器捲動要關掉')
})

test('Esc 關浮層時把事件標成已處理（側欄抽屜的 Esc 不會一起關）', async () => {
  freshBadgeState()
  seed([row({ id: 'esc-2', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.9 })])
  const root = await mount(<JudgeBadge botId="bot-a" />)
  assert.ok(await openBadge(root))
  const seen: boolean[] = []
  const probe = (e: KeyboardEvent) => seen.push(e.defaultPrevented)
  window.addEventListener('keydown', probe)
  try {
    await pressKey('Escape', { cancelable: true })
  } finally {
    window.removeEventListener('keydown', probe)
  }
  await settle(20)
  assert.deepEqual(seen, [true], '冒泡到 window 時要已經 preventDefault')
  assert.equal(root.querySelector('.judge-badge-pop'), null)
})
