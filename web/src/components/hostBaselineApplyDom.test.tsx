/**
 * 主機一致性計畫一鍵補齊（HostBaselineApply，#719 階段 3b）：
 * - sync=off：不畫鈕
 * - ask/auto 且有 fixable 項：畫「補齊 N 項」鈕
 * - 確認框：明列主機名、要補的 id、目標檔、「會先備份、只補不蓋」
 * - 預設 focus 在取消鈕，不在確認鈕上；Enter 不能直接送出
 * - 確認後打 API 帶 plan_rev 與 ids
 * - 409（plan_outdated）：重新抓計畫，顯示「計畫已更新，請重新確認」，不自動重送
 * - 錯誤只顯示錯誤碼加固定文案，不顯示主機內容
 * - 成功顯示 backup_id 與各項結果
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, keydown, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { HostBaselineApply } from './HostBaselineApply'
import type { HostBaseline, HostBaselinePlan } from '../api/types'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  await teardownDom()
})

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 10_000 }, fn)

const sampleBaseline: HostBaseline = {
  issues: [
    { id: 'claude.hooks', severity: 'critical', message: 'settings.json 沒有 hooks' },
    { id: 'claude.perm', severity: 'warn', message: 'settings.json 沒有 permissions' },
  ],
  fixable: ['claude.hooks', 'claude.perm'],
  checked_at: new Date().toISOString(),
  os: 'Linux',
  stale: false,
  error: null,
}

const samplePlan: HostBaselinePlan = {
  host: 'worker-1',
  checked_at: new Date().toISOString(),
  stale: false,
  plan_rev: 'rev-12345',
  actions: [
    {
      id: 'claude.hooks',
      action: 'merge_key',
      severity: 'critical',
      target: '~/.claude/settings.json',
      key: 'hooks',
    },
    {
      id: 'claude.perm',
      action: 'merge_key',
      severity: 'warn',
      target: '~/.claude/settings.json',
      key: 'permissions',
    },
  ],
}

it('sync=off：不畫按鈕', async () => {
  await mount(<HostBaselineApply host="worker-1" baseline={sampleBaseline} syncMode="off" />)
  const btn = document.querySelector('.host-baseline-apply-btn')
  assert.equal(btn, null)
})

it('無 fixable 項目：不畫按鈕', async () => {
  const noFixable: HostBaseline = {
    ...sampleBaseline,
    fixable: [],
  }
  await mount(<HostBaselineApply host="worker-1" baseline={noFixable} syncMode="ask" />)
  const btn = document.querySelector('.host-baseline-apply-btn')
  assert.equal(btn, null)
})

it('sync=ask 或 sync=auto 且有 fixable 項：畫「補齊 N 項」鈕', async () => {
  await mount(<HostBaselineApply host="worker-1" baseline={sampleBaseline} syncMode="ask" />)
  let btn = document.querySelector('.host-baseline-apply-btn')
  assert.ok(btn)
  assert.match(btn.textContent ?? '', /補齊 2 項/)

  await unmountAll()

  await mount(<HostBaselineApply host="worker-1" baseline={sampleBaseline} syncMode="auto" />)
  btn = document.querySelector('.host-baseline-apply-btn')
  assert.ok(btn)
  assert.match(btn.textContent ?? '', /補齊 2 項/)
})

it('二次確認：明列主機名、要補的 id、目標檔、「會先備份、只補不蓋」；預設 focus 不在確認鈕，Enter 不直接送出', async () => {
  const origFetch = globalThis.fetch
  let fetchCalled = 0
  let applyCalled = 0

  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const url = typeof input === 'string' ? input : input.toString()
    if (url.includes('/api/hosts/worker-1/baseline/plan')) {
      fetchCalled++
      return new Response(JSON.stringify(samplePlan), {
        status: 200,
        headers: { 'Content-Type': 'application/json' },
      })
    }
    if (url.includes('/api/hosts/worker-1/baseline/apply')) {
      applyCalled++
      return new Response(JSON.stringify({ backup_id: 'bk-1', results: [] }), {
        status: 200,
        headers: { 'Content-Type': 'application/json' },
      })
    }
    return new Response('{}', { status: 200 })
  }) as unknown as typeof fetch

  try {
    await mount(<HostBaselineApply host="worker-1" baseline={sampleBaseline} syncMode="ask" />)
    const btn = document.querySelector<HTMLButtonElement>('.host-baseline-apply-btn')!
    await click(btn)

    assert.equal(fetchCalled, 1)

    // 對話框已開啟
    const dialog = document.querySelector('[role=alertdialog]')!
    assert.ok(dialog)

    // 明列主機名、目標檔、只補不蓋
    assert.match(dialog.textContent ?? '', /主機：worker-1/)
    assert.match(dialog.textContent ?? '', /會先備份、只補不蓋/)
    assert.match(dialog.textContent ?? '', /claude\.hooks/)
    assert.match(dialog.textContent ?? '', /claude\.perm/)
    assert.match(dialog.textContent ?? '', /目標：~\/\.claude\/settings\.json/)

    // 焦點檢驗：預設 focus 在「取消」鈕，不在「確認補齊」
    const confirmBtn = [...dialog.querySelectorAll<HTMLButtonElement>('.confirm-actions button')].find((b) => b.textContent?.includes('確認補齊'))!
    const cancelBtn = [...dialog.querySelectorAll<HTMLButtonElement>('.confirm-actions button')].find((b) => b.textContent?.includes('取消'))!
    assert.ok(confirmBtn)
    assert.ok(cancelBtn)
    assert.equal(document.activeElement, cancelBtn, '預設焦點必須在取消鈕')
    assert.notEqual(document.activeElement, confirmBtn, '確認鈕不准預設 focus')

    // 按 Enter 落在取消鈕上，只會取消，不會觸發 apply
    await keydown(document.activeElement!, 'Enter')
    assert.equal(applyCalled, 0, 'Enter 落在取消鈕上不能觸發 apply')
  } finally {
    globalThis.fetch = origFetch
  }
})

it('確認後呼叫 applyHostBaseline 帶 plan_rev 與 ids', async () => {
  const origFetch = globalThis.fetch
  let appliedBody: { plan_rev: string; ids: string[] } | null = null

  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === 'string' ? input : input.toString()
    if (url.includes('/api/hosts/worker-1/baseline/plan')) {
      return new Response(JSON.stringify(samplePlan), {
        status: 200,
        headers: { 'Content-Type': 'application/json' },
      })
    }
    if (url.includes('/api/hosts/worker-1/baseline/apply')) {
      appliedBody = JSON.parse(init?.body as string)
      return new Response(
        JSON.stringify({
          backup_id: '20261025T010203Z-bk01',
          results: [
            { id: 'claude.hooks', result: 'applied' },
            { id: 'claude.perm', result: 'applied' },
          ],
        }),
        {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        },
      )
    }
    return new Response('{}', { status: 200 })
  }) as unknown as typeof fetch

  try {
    await mount(<HostBaselineApply host="worker-1" baseline={sampleBaseline} syncMode="ask" />)
    const btn = document.querySelector<HTMLButtonElement>('.host-baseline-apply-btn')!
    await click(btn)

    const dialog = document.querySelector('[role=alertdialog]')!
    const confirmBtn = [...dialog.querySelectorAll<HTMLButtonElement>('.confirm-actions button')].find((b) => b.textContent?.includes('確認補齊'))!
    await click(confirmBtn)

    assert.deepEqual(appliedBody, {
      plan_rev: 'rev-12345',
      ids: ['claude.hooks', 'claude.perm'],
    })

    // 對話框關閉
    assert.equal(document.querySelector('[role=alertdialog]'), null)

    // 成功結果顯示 backup_id 與各項結果
    const root = document.querySelector('.host-baseline-apply-wrap')!
    assert.match(root.textContent ?? '', /備份 ID：20261025T010203Z-bk01/)
    assert.match(root.textContent ?? '', /（如需還原請使用命令列）/)
    assert.match(root.textContent ?? '', /claude\.hooks: 已補齊/)
    assert.match(root.textContent ?? '', /claude\.perm: 已補齊/)
  } finally {
    globalThis.fetch = origFetch
  }
})

it('409 plan_outdated：重新抓計畫，顯示「計畫已更新，請重新確認」，不准自動重送 apply', async () => {
  const origFetch = globalThis.fetch
  let fetchCount = 0
  let applyCount = 0

  const plan2: HostBaselinePlan = {
    host: 'worker-1',
    checked_at: new Date().toISOString(),
    stale: false,
    plan_rev: 'rev-99999',
    actions: [
      {
        id: 'claude.hooks',
        action: 'merge_key',
        severity: 'critical',
        target: '~/.claude/settings.json',
        key: 'hooks',
      },
    ],
  }

  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const url = typeof input === 'string' ? input : input.toString()
    if (url.includes('/api/hosts/worker-1/baseline/plan')) {
      fetchCount++
      const p = fetchCount === 1 ? samplePlan : plan2
      return new Response(JSON.stringify(p), {
        status: 200,
        headers: { 'Content-Type': 'application/json' },
      })
    }
    if (url.includes('/api/hosts/worker-1/baseline/apply')) {
      applyCount++
      return new Response(JSON.stringify({ error: 'conflict', reason: 'plan_outdated' }), {
        status: 409,
        headers: { 'Content-Type': 'application/json' },
      })
    }
    return new Response('{}', { status: 200 })
  }) as unknown as typeof fetch

  try {
    await mount(<HostBaselineApply host="worker-1" baseline={sampleBaseline} syncMode="ask" />)
    const btn = document.querySelector<HTMLButtonElement>('.host-baseline-apply-btn')!
    await click(btn)

    assert.equal(fetchCount, 1)

    const dialog = document.querySelector('[role=alertdialog]')!
    const confirmBtn = [...dialog.querySelectorAll<HTMLButtonElement>('.confirm-actions button')].find((b) => b.textContent?.includes('確認補齊'))!
    await click(confirmBtn)

    assert.equal(applyCount, 1, '只打一次 apply，不准自動重送')
    assert.equal(fetchCount, 2, '必須重新抓取新計畫')

    // 對話框維持開啟，顯示固定文案提示
    assert.ok(document.querySelector('[role=alertdialog]'))
    const errBox = document.querySelector('.host-baseline-apply-err')
    assert.ok(errBox)
    assert.equal(errBox.textContent, '計畫已更新，請重新確認')

    // 更新後的計畫項目呈現在對話框內（只有 claude.hooks，無 claude.perm）
    const dialogText = document.querySelector('[role=alertdialog]')!.textContent ?? ''
    assert.match(dialogText, /claude\.hooks/)
    assert.doesNotMatch(dialogText, /claude\.perm/)
  } finally {
    globalThis.fetch = origFetch
  }
})

it('錯誤只顯示錯誤碼加固定文案，不顯示主機上的任何檔案內容', async () => {
  const origFetch = globalThis.fetch

  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const url = typeof input === 'string' ? input : input.toString()
    if (url.includes('/api/hosts/worker-1/baseline/plan')) {
      return new Response(JSON.stringify(samplePlan), {
        status: 200,
        headers: { 'Content-Type': 'application/json' },
      })
    }
    if (url.includes('/api/hosts/worker-1/baseline/apply')) {
      return new Response(
        JSON.stringify({
          error: 'internal_error',
          reason: 'backup_failed',
          raw_output: 'SECRET_SSH_KEY_CONTENT_DO_NOT_LEAK',
        }),
        {
          status: 500,
          headers: { 'Content-Type': 'application/json' },
        },
      )
    }
    return new Response('{}', { status: 200 })
  }) as unknown as typeof fetch

  try {
    await mount(<HostBaselineApply host="worker-1" baseline={sampleBaseline} syncMode="ask" />)
    const btn = document.querySelector<HTMLButtonElement>('.host-baseline-apply-btn')!
    await click(btn)

    const dialog = document.querySelector('[role=alertdialog]')!
    const confirmBtn = [...dialog.querySelectorAll<HTMLButtonElement>('.confirm-actions button')].find((b) => b.textContent?.includes('確認補齊'))!
    await click(confirmBtn)

    // 檢查畫面中絕對不包含 sensitive 主機檔案內容
    const root = document.querySelector('.host-baseline-apply-wrap')!
    assert.doesNotMatch(root.textContent ?? '', /SECRET_SSH_KEY_CONTENT_DO_NOT_LEAK/)
    assert.match(root.textContent ?? '', /主機備份失敗，未寫入變更/)
  } finally {
    globalThis.fetch = origFetch
  }
})
