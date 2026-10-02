import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import type { Run } from '../api/types.ts'
import { LAMP_LABEL, botStateLabel } from './lampLabel.ts'
import { BotStateText } from './BotStateText.tsx'

const run = (over: Partial<Run> = {}) =>
  ({ ...({} as Run), state: 'running', agent_status: 'blocked', blocked_reason: { code: 'codex_update_menu', text: 'codex 更新提示等待選擇' }, ...over }) as Run

test('狀態字：blocked 帶原因時，tooltip／標題燈寫「等待回應：原因」，跟側欄同一個字串', () => {
  assert.equal(botStateLabel('blocked', run()), `${LAMP_LABEL.blocked}：codex 更新提示等待選擇`)
  assert.equal(botStateLabel('blocked', run({ blocked_reason: null })), LAMP_LABEL.blocked, '沒有原因照舊')
  assert.equal(botStateLabel('blocked', null), LAMP_LABEL.blocked)
  assert.equal(botStateLabel('idle', run({ agent_status: 'idle' })), LAMP_LABEL.idle, '只有 blocked 才帶原因')
  assert.equal(botStateLabel('working', run({ agent_status: 'working' })), LAMP_LABEL.working)
})

test('側欄的狀態字：blocked 的旁邊多一行小字原因（hover 也有）；沒有原因或不是 blocked 跟以前一樣', () => {
  const html = renderToStaticMarkup(<BotStateText lamp="blocked" reason="codex 更新提示等待選擇" />)
  assert.match(html, /bot-state blocked/)
  assert.ok(html.includes(LAMP_LABEL.blocked), html)
  assert.match(html, /class="bot-state-reason"[^>]*>codex 更新提示等待選擇</)
  assert.match(html, /title="codex 更新提示等待選擇"/)
  const plain = renderToStaticMarkup(<BotStateText lamp="blocked" reason="" />)
  assert.doesNotMatch(plain, /bot-state-reason/)
  assert.doesNotMatch(renderToStaticMarkup(<BotStateText lamp="idle" reason="不該出現" />), /bot-state-reason/, '只有 blocked 才顯示')
})
