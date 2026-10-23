/**
 * 環境設定的分頁外殼（EnvShell，issue #1922）：
 * 桌機（>640px）左側分頁 + 內頁單一捲軸；手機（≤640px）標題下折行分頁。
 * 七個分頁維持掛載，切換時以 hidden 隱藏，保持填寫中的表單與捲動位置。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { EnvShell } from './EnvShell'
import { clearRememberedTab, rememberTab } from '../lib/envTabs'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

beforeEach(() => {
  clearRememberedTab()
})

test('七個分頁都在 DOM 裡，預設選中主機，其餘 6 個面板帶 hidden', async () => {
  const root = await mount(<EnvShell />)
  const tabs = [...root.querySelectorAll<HTMLButtonElement>('.env-nav[role="tablist"] button[role="tab"]')]
  assert.equal(tabs.length, 7)
  assert.deepEqual(
    tabs.map((t) => t.querySelector('.env-nav-label')?.textContent),
    ['主機', '更新檢查', '身分', '最近刪除', 'Jev 建議', 'Claude API', '顯示'],
  )

  const selected = tabs.find((t) => t.getAttribute('aria-selected') === 'true')
  assert.ok(selected)
  assert.equal(selected.querySelector('.env-nav-label')?.textContent, '主機')

  const panels = [...root.querySelectorAll<HTMLElement>('.env-main > [role="tabpanel"]')]
  assert.equal(panels.length, 7)
  const visiblePanels = panels.filter((p) => !p.hidden)
  assert.equal(visiblePanels.length, 1)
  assert.equal(visiblePanels[0]!.id, 'env-panel-hosts')

  const hiddenPanels = panels.filter((p) => p.hidden)
  assert.equal(hiddenPanels.length, 6)
})

test('點擊切換分頁：更新 aria-selected 與 hidden，記住切換狀態', async () => {
  const root = await mount(<EnvShell />)
  const claudeTab = root.querySelector<HTMLButtonElement>('#env-tab-claude-api')
  assert.ok(claudeTab)

  await click(claudeTab)

  assert.equal(claudeTab.getAttribute('aria-selected'), 'true')
  const claudePanel = root.querySelector<HTMLElement>('#env-panel-claude-api')
  assert.ok(claudePanel && !claudePanel.hidden)

  const hostsPanel = root.querySelector<HTMLElement>('#env-panel-hosts')
  assert.ok(hostsPanel?.hidden)
})

test('記住上次停的分頁：若 session 已有記憶，初次打開直接停在該分頁', async () => {
  rememberTab('display')
  const root = await mount(<EnvShell />)

  const displayTab = root.querySelector<HTMLButtonElement>('#env-tab-display')
  assert.ok(displayTab)
  assert.equal(displayTab.getAttribute('aria-selected'), 'true')

  const displayPanel = root.querySelector<HTMLElement>('#env-panel-display')
  assert.ok(displayPanel && !displayPanel.hidden)
})
