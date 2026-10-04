/**
 * #714：回合結束但背景 shell 還在跑，不能「看起來像停了」。側欄以外的地方（標題燈、BotSwitcher、群組成員、
 * 手機主力晶片）原本只寫「閒置」；現在全部走 `botStateLabel`／`StatusLamp` 的 `background`，文案與側欄同一來源。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import type { Run } from '../api/types.ts'
import { backgroundLabel } from '../lib/backgroundJobs.ts'
import { LAMP_LABEL, botStateLabel } from './lampLabel.ts'
import { StatusLamp } from './StatusLamp.tsx'

const run = (over: Partial<Run> = {}) => ({ ...({} as Run), state: 'running', agent_status: 'idle', background_jobs: 2, ...over }) as Run

test('閒置但背景還有工作：文案是「背景執行中（N）」，跟側欄同一個字串', () => {
  assert.equal(botStateLabel('idle', run()), backgroundLabel(2))
  assert.equal(botStateLabel('idle', run({ background_jobs: 0 })), LAMP_LABEL.idle)
  assert.equal(botStateLabel('idle', null), LAMP_LABEL.idle)
})

test('只有燈號是 idle 才改字：執行中、等待回應、斷線不被背景數字蓋掉', () => {
  assert.equal(botStateLabel('working', run({ agent_status: 'working' })), LAMP_LABEL.working)
  assert.equal(botStateLabel('blocked', run({ agent_status: 'blocked' })), LAMP_LABEL.blocked)
  assert.equal(botStateLabel('disconnected', run()), LAMP_LABEL.disconnected, '斷線時 daemon 讀不到畫面，背景數字不可信')
})

test('StatusLamp：只有背景工作不轉圈（字寫背景執行中）；閒著但子 agent 在跑才轉', () => {
  // 2026-10-04 使用者：「沒有 child 也在轉，轉個毛」。
  const html = renderToStaticMarkup(<StatusLamp lamp="idle" background={3} />)
  assert.doesNotMatch(html, /lamp-bg/, '只有背景 shell：一般綠點')
  assert.match(renderToStaticMarkup(<StatusLamp lamp="idle" kids={2} />), /lamp-bg[^>]*子 agent 還在跑（2）|子 agent 還在跑（2）[^>]*lamp-bg|lamp-bg/)
  assert.ok(renderToStaticMarkup(<StatusLamp lamp="idle" kids={2} />).includes('子 agent 還在跑（2）'))
  assert.doesNotMatch(renderToStaticMarkup(<StatusLamp lamp="working" kids={2} />), /lamp-bg/, '自己在跑就是 working 燈，不疊轉圈')
  assert.ok(html.includes(`aria-label="${backgroundLabel(3)}"`), html)
  assert.ok(html.includes(`title="${backgroundLabel(3)}"`), html)
  // 沒有背景工作、或不是 idle：跟以前一樣。
  assert.doesNotMatch(renderToStaticMarkup(<StatusLamp lamp="idle" />), /lamp-bg/)
  assert.doesNotMatch(renderToStaticMarkup(<StatusLamp lamp="working" background={3} />), /lamp-bg/)
  // 呼叫端給了 title（例如「名字：狀態」）就用它。
  assert.match(renderToStaticMarkup(<StatusLamp lamp="idle" background={3} title="a：背景執行中（3）" />), /title="a：背景執行中（3）"/)
})
