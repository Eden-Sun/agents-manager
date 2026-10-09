/**
 * 換 bot 選單的鍵盤行為（真的掛進 happy-dom）：#959 打開後焦點要落在目前那顆上，方向鍵才有作用。
 * 選單等 `pos` 算完才畫的話，`useMenuKeys` 的 effect 會先看到 `null` 就 return，焦點永遠進不去。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, keydown, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, Project } from '../api/types'
import { BotSwitcher } from './BotSwitcher'

afterEach(unmountAll)
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})

function seed() {
  resetStoreForTest()
  useStore.setState({
    projects: [{ id: 'p1', label: 'proj', path: '/p', host: 'm4p' } as Project],
    bots: [
      { id: 'b1', name: 'one', project_id: 'p1', kind: 'claude' } as Bot,
      { id: 'b2', name: 'two', project_id: 'p1', kind: 'codex' } as Bot,
    ],
  })
}

test('換 bot 選單打開後焦點落在目前那顆（aria-checked）上，↓ 移到下一顆', async () => {
  seed()
  await mount(<BotSwitcher botId="b1" name="one" />)
  const btn = document.querySelector<HTMLButtonElement>('.bot-switcher-btn')!
  await click(btn)
  // 等一格：useMenuKeys 用 requestAnimationFrame 等選單畫出來再 focus。
  await settle(20)
  const current = document.activeElement as HTMLElement
  assert.equal(current.getAttribute('role'), 'menuitemradio', '焦點要進選單')
  assert.equal(current.getAttribute('aria-checked'), 'true')
  assert.equal(current.textContent?.includes('one'), true)
  await keydown(current, 'ArrowDown')
  assert.equal((document.activeElement as HTMLElement).textContent?.includes('two'), true, '↓ 要把焦點移到下一顆')
})
