/**
 * 主機工作環境一致性收合（BaselineLine，#1922）：
 * - 嚴重項維持展開
 * - 提醒項進 <details class="env-fold">，有嚴重項時 summary 為「還有 N 項提醒」，無嚴重項時為「列出 N 項」
 * - 未知與尚未檢查不上色、不收進摺疊
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest } from '../store/store'
import { BaselineLine } from './HostsPanel'
import type { HostBaseline } from '../api/types'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 10_000 }, fn)

it('尚未檢查：無 items，不上色、不收進 details', async () => {
  await mount(<BaselineLine baseline={null} />)
  const root = document.querySelector('.host-baseline')!
  assert.ok(root)
  assert.equal(root.querySelector('.host-baseline-head')?.textContent, '一致性：尚未檢查')
  assert.equal(root.querySelectorAll('.host-baseline-items li').length, 0)
  assert.equal(root.querySelector('details'), null)
})

it('嚴重項展開，提醒項收進 details summary「還有 N 項提醒」', async () => {
  const baseline: HostBaseline = {
    issues: [
      { id: 'git.status', severity: 'critical', message: '缺 statusline-command.sh' },
      { id: 'claude.hooks', severity: 'critical', message: 'settings.json 沒有 hooks' },
      { id: 'claude.perm', severity: 'warn', message: 'settings.json 沒有 permissions' },
      { id: 'claude.mode', severity: 'warn', message: 'settings.json 沒有 defaultMode' },
    ],
    checked_at: new Date().toISOString(),
    os: 'Linux',
    stale: false,
    error: null,
  }
  await mount(<BaselineLine baseline={baseline} />)
  const root = document.querySelector('.host-baseline')!
  assert.ok(root)
  assert.match(root.querySelector('.host-baseline-head')?.textContent ?? '', /4 項不一致（2 項嚴重）/)

  // 嚴重項直接在外層 ul
  const critUl = root.querySelector('ul.host-baseline-items')!
  assert.ok(critUl)
  const critLis = critUl.querySelectorAll('li')
  assert.equal(critLis.length, 2)
  assert.equal(critLis[0].textContent, '缺 statusline-command.sh')
  assert.equal(critLis[0].className, 'critical')
  assert.equal(critLis[1].textContent, 'settings.json 沒有 hooks')
  assert.equal(critLis[1].className, 'critical')

  // 提醒項在 details.env-fold 裡
  const details = root.querySelector<HTMLDetailsElement>('details.env-fold')!
  assert.ok(details)
  assert.equal(details.open, false, '預設關')
  assert.equal(details.querySelector('summary')?.textContent, '還有 2 項提醒')
  const warnLis = details.querySelectorAll('li')
  assert.equal(warnLis.length, 2)
  assert.equal(warnLis[0].textContent, 'settings.json 沒有 permissions')
  assert.equal(warnLis[0].className, 'warn')
})

it('沒有嚴重項、只有提醒項時：清單整段放進 details「列出 N 項」，預設關', async () => {
  const baseline: HostBaseline = {
    issues: [
      { id: 'claude.perm', severity: 'warn', message: 'settings.json 沒有 permissions' },
      { id: 'claude.mode', severity: 'warn', message: 'settings.json 沒有 defaultMode' },
    ],
    checked_at: new Date().toISOString(),
    os: 'Linux',
    stale: false,
    error: null,
  }
  await mount(<BaselineLine baseline={baseline} />)
  const root = document.querySelector('.host-baseline')!
  assert.ok(root)
  assert.match(root.querySelector('.host-baseline-head')?.textContent ?? '', /2 項提醒/)

  // 外層沒有直接展開的 items
  assert.equal(root.children.length, 2) // .host-baseline-head + details.env-fold

  const details = root.querySelector<HTMLDetailsElement>('details.env-fold')!
  assert.ok(details)
  assert.equal(details.open, false, '預設關')
  assert.equal(details.querySelector('summary')?.textContent, '列出 2 項')
  const warnLis = details.querySelectorAll('li')
  assert.equal(warnLis.length, 2)
})

it('與基準一致：無 details 也無 items', async () => {
  const baseline: HostBaseline = {
    issues: [],
    checked_at: new Date().toISOString(),
    os: 'Linux',
    stale: false,
    error: null,
  }
  await mount(<BaselineLine baseline={baseline} />)
  const root = document.querySelector('.host-baseline')!
  assert.ok(root)
  assert.match(root.querySelector('.host-baseline-head')?.textContent ?? '', /與基準一致/)
  assert.equal(root.querySelector('details'), null)
  assert.equal(root.querySelectorAll('li').length, 0)
})

it('#719 階段 2：每項尾端標「可自動補／需手動」；沒有 fixable 不標', async () => {
  const baseline: HostBaseline = {
    issues: [
      { id: 'tool.rtk', severity: 'critical', message: '缺工具 rtk' },
      { id: 'codex.config.toml:features.hooks', severity: 'warn', message: 'codex：config.toml 沒有 features.hooks' },
    ],
    checked_at: new Date().toISOString(),
    os: 'Linux',
    stale: false,
    error: null,
    fixable: ['codex.config.toml:features.hooks'],
  }
  await mount(<BaselineLine baseline={baseline} />)
  const crit = document.querySelector('ul.host-baseline-items li')!
  assert.equal(crit.querySelector('.host-baseline-fix')?.className, 'host-baseline-fix manual')
  assert.equal(crit.textContent, '缺工具 rtk（需手動）')
  const warn = document.querySelector('details.env-fold li')!
  assert.equal(warn.querySelector('.host-baseline-fix')?.className, 'host-baseline-fix auto')
  assert.equal(warn.textContent, 'codex：config.toml 沒有 features.hooks（可自動補）')
  await unmountAll()

  await mount(<BaselineLine baseline={{ ...baseline, fixable: undefined }} />)
  assert.equal(document.querySelectorAll('.host-baseline-fix').length, 0)
})
