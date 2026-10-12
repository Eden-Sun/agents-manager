/**
 * 狀態列（context／快取倒數／版本）不是 live region：它會隨回合與快取倒數變，role=status 會讓讀屏每次都重念。
 * 要念的事（blocked、錯誤、回覆）各自有自己的 live 區。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest } from '../store/store'
import type { StatusInfo } from '../api/types'
import { StatusLineBar } from './ChatPanel'

before(setupDom)
afterEach(async () => {
  await unmountAll()
  resetStoreForTest()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const status = {
  account_email: null, account_warning: null, model_name: 'Opus', model_id: 'opus', effort: 'medium', thinking: false, fast_mode: false,
  context_used_pct: 42, context_used_tokens: 84000, context_size: 200000, five_hour_pct: null, five_hour_resets_at: null,
  seven_day_pct: null, seven_day_resets_at: null, cost_usd: null, cwd: '/home/you/project', version: '2.1.300', session_name: null,
} as StatusInfo

test('狀態列不是 live region：沒有 role=status，也沒有 aria-live', async () => {
  await mount(<StatusLineBar botId="b1" status={status} text={null} />)
  const bar = document.querySelector<HTMLElement>('.statusline-bar')!
  assert.ok(bar, '要畫出狀態列')
  assert.equal(bar.getAttribute('role'), null, '狀態列會一直變，不能宣告成 status')
  assert.equal(bar.hasAttribute('aria-live'), false)
})
