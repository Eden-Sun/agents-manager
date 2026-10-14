/**
 * #714：回合結束但背景 shell 還在跑，不能「看起來像停了」。側欄以外的地方（標題燈、BotSwitcher、群組成員、
 * 手機主力晶片）原本只寫「閒置」；現在全部走 `botStateLabel`／`StatusLamp` 的 `background`，文案與側欄同一來源。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import type { Run } from '../api/types.ts'
import { backgroundLabel } from '../lib/backgroundJobs.ts'
import { COMPACTING_LABEL, LAMP_LABEL, botStateLabel, isCompacting } from './lampLabel.ts'
import { BotStateText } from './BotStateText.tsx'
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

/**
 * issue #1770：claude 壓縮對話（`PreCompact` 標成 `compacting_since`）時，側欄與對話區寫「壓縮中…」，跟「執行中」分開；
 * 燈號照舊（working／idle），斷線與等待回應不被蓋掉；沒有標記或舊 daemon（沒這個欄位）一律照舊文案。
 */
test('壓縮中：執行中或閒置時文案是「壓縮中…」，燈號與其他狀態不被蓋掉', () => {
  const compacting = run({ agent_status: 'working', compacting_since: '2026-10-14T18:00:00.000Z' })
  assert.equal(botStateLabel('working', compacting), COMPACTING_LABEL)
  assert.equal(botStateLabel('idle', run({ compacting_since: '2026-10-14T18:00:00.000Z', background_jobs: 2 })), COMPACTING_LABEL, '壓縮中排在背景數字前面')
  assert.equal(botStateLabel('blocked', run({ agent_status: 'blocked', compacting_since: '2026-10-14T18:00:00.000Z' })), LAMP_LABEL.blocked)
  assert.equal(botStateLabel('disconnected', compacting), LAMP_LABEL.disconnected)
  assert.equal(botStateLabel('working', run({ agent_status: 'working', compacting_since: null })), LAMP_LABEL.working, '沒有標記照舊')
  assert.equal(isCompacting('working', null), false)
})

test('側欄狀態字：壓縮中時寫「壓縮中…」，不是「執行中」', () => {
  assert.match(renderToStaticMarkup(<BotStateText lamp="working" reason="" compacting />), /壓縮中…/)
  assert.doesNotMatch(renderToStaticMarkup(<BotStateText lamp="working" reason="" compacting />), /執行中/)
  assert.match(renderToStaticMarkup(<BotStateText lamp="working" reason="" />), /執行中/, '沒有壓縮就照舊')
})
