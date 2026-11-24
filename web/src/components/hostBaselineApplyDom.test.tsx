import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, keydown, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { HostBaselineApply } from './HostBaselineApply'
import { toHostBaselineSync, toState } from '../api/normalize'
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

it('sync=ask 且有 fixable 項：畫「補齊 N 項」鈕', async () => {
  await mount(<HostBaselineApply host="worker-1" baseline={sampleBaseline} syncMode="ask" />)
  const btn = document.querySelector('.host-baseline-apply-btn')
  assert.ok(btn)
  assert.match(btn.textContent ?? '', /補齊 2 項/)
})

it('沒有 host_baseline_sync 這欄（舊 daemon、bot 視圖）：一律當 off，不畫按鈕', async () => {
  const sync = toState({}).host_baseline_sync
  assert.deepEqual(sync, {})
  await mount(<HostBaselineApply host="worker-1" baseline={sampleBaseline} syncMode={sync['worker-1']} />)
  assert.equal(document.querySelector('.host-baseline-apply-btn'), null)
})

it('host_baseline_sync 正規化：ask／auto 保留，其他值都是 off', () => {
  assert.deepEqual(toHostBaselineSync({ local: 'ask', a: 'off', b: 'auto', c: 1, d: null }), {
    local: 'ask',
    a: 'off',
    b: 'auto',
    c: 'off',
    d: 'off',
  })
  assert.deepEqual(toHostBaselineSync(['ask']), {})
  assert.deepEqual(toHostBaselineSync(undefined), {})
  return Promise.resolve()
})

it('sync=auto 且有 fixable 項：有明確狀態、不畫手動補齊鈕', async () => {
  await mount(<HostBaselineApply host="worker-1" baseline={sampleBaseline} syncMode="auto" />)
  assert.equal(document.querySelector('.host-baseline-apply-btn'), null)
  const status = document.querySelector('.host-baseline-auto-status')
  assert.ok(status)
  assert.match(status.textContent ?? '', /自動模式/)
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

    const dialog = document.querySelector('[role=alertdialog]')!
    assert.ok(dialog)

    assert.match(dialog.textContent ?? '', /主機：worker-1/)
    assert.match(dialog.textContent ?? '', /會先備份、只補不蓋/)
    assert.match(dialog.textContent ?? '', /claude\.hooks/)
    assert.match(dialog.textContent ?? '', /claude\.perm/)
    assert.match(dialog.textContent ?? '', /目標：~\/\.claude\/settings\.json/)

    const confirmBtn = [...dialog.querySelectorAll<HTMLButtonElement>('.confirm-actions button')].find((b) => b.textContent?.includes('確認補齊'))!
    const cancelBtn = [...dialog.querySelectorAll<HTMLButtonElement>('.confirm-actions button')].find((b) => b.textContent?.includes('取消'))!
    assert.ok(confirmBtn)
    assert.ok(cancelBtn)
    assert.equal(document.activeElement, cancelBtn, '預設焦點必須在取消鈕')
    assert.notEqual(document.activeElement, confirmBtn, '確認鈕不准預設 focus')

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

    assert.equal(document.querySelector('[role=alertdialog]'), null)

    const root = document.querySelector('.host-baseline-apply-wrap')!
    assert.match(root.textContent ?? '', /備份 ID：20261025T010203Z-bk01/)
    assert.match(root.textContent ?? '', /可用這個備份 ID 還原/)
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

    assert.ok(document.querySelector('[role=alertdialog]'))
    const errBox = document.querySelector('.host-baseline-apply-err')
    assert.ok(errBox)
    assert.equal(errBox.textContent, '計畫已更新，請重新確認')

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

    const root = document.querySelector('.host-baseline-apply-wrap')!
    assert.doesNotMatch(root.textContent ?? '', /SECRET_SSH_KEY_CONTENT_DO_NOT_LEAK/)
    assert.match(root.textContent ?? '', /主機備份失敗，未寫入變更/)
  } finally {
    globalThis.fetch = origFetch
  }
})

it('#719 階段 3：確認框分「將寫入／將安裝／需手動」，只送可套用的 id', async () => {
  const origFetch = globalThis.fetch
  let appliedBody: { plan_rev: string; ids: string[] } | null = null
  const plan: HostBaselinePlan = {
    host: 'worker-1',
    checked_at: new Date().toISOString(),
    stale: false,
    plan_rev: 'rev-3',
    actions: [
      { id: 'claude.cc1.RTK.md', action: 'write_file', severity: 'warn', target: 'claude:cc1/RTK.md' },
      { id: 'tool.rtk', action: 'install_tool', severity: 'warn', target: 'tool/rtk', command: 'brew install rtk' },
      { id: 'tool.jq', action: 'manual', severity: 'warn', target: 'tool/jq', reason: 'needs_sudo', command: 'sudo apt install jq' },
    ],
  }
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === 'string' ? input : input.toString()
    if (url.includes('/baseline/plan')) return new Response(JSON.stringify(plan), { status: 200, headers: { 'Content-Type': 'application/json' } })
    if (url.includes('/baseline/apply')) {
      appliedBody = JSON.parse(init?.body as string)
      return new Response(JSON.stringify({ backup_id: null, results: [{ id: 'tool.rtk', result: 'skipped', reason: 'install_failed' }] }), { status: 200, headers: { 'Content-Type': 'application/json' } })
    }
    return new Response('{}', { status: 200 })
  }) as unknown as typeof fetch
  try {
    await mount(<HostBaselineApply host="worker-1" baseline={{ ...sampleBaseline, fixable: ['claude.cc1.RTK.md', 'tool.rtk'] }} syncMode="ask" />)
    await click(document.querySelector<HTMLButtonElement>('.host-baseline-apply-btn')!)
    const dialog = document.querySelector('[role=alertdialog]')!
    const text = dialog.textContent ?? ''
    assert.match(text, /將寫入/)
    assert.match(text, /將安裝/)
    assert.match(text, /需手動/)
    assert.match(text, /brew install rtk/)
    assert.match(text, /sudo apt install jq/)
    const confirmBtn = [...dialog.querySelectorAll<HTMLButtonElement>('.confirm-actions button')].find((b) => b.textContent?.includes('確認補齊'))!
    await click(confirmBtn)
    assert.deepEqual(appliedBody, { plan_rev: 'rev-3', ids: ['claude.cc1.RTK.md', 'tool.rtk'] })
    assert.match(document.body.textContent ?? '', /略過（安裝失敗）/)
  } finally {
    globalThis.fetch = origFetch
  }
})
