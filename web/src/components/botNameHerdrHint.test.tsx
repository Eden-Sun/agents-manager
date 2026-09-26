import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { BotNameField } from './BotNameField.tsx'
import { herdrIdentity } from '../lib/herdrIdentity.ts'
import { HerdrAgentName } from './HerdrAgentName.tsx'

// #572：側欄只畫暱稱，滑過去要看得到終端那邊的 herdr 名與 pane，才對得起來。
const row = (run: { pane_id: string | null } | null, armed = false) =>
  renderToStaticMarkup(
    <BotNameField botId="b1" name="cf-ox-fork-fork" variant="row" armed={armed} hint={herdrIdentity(run, 'pt-hub-n4xznj')?.title} />,
  )

test('側欄暱稱：有 active run 時 tooltip 帶 herdr 名與 pane id，列上仍只畫暱稱', () => {
  const html = row({ pane_id: 'w16T:p3' })
  assert.match(html, /title="cf-ox-fork-fork\nherdr agent pt-hub-n4xznj・pane w16T:p3"/)
  assert.match(html, />cf-ox-fork-fork</)
  assert.match(row({ pane_id: 'w16T:p3' }, true), /title="cf-ox-fork-fork · 點一下改名\nherdr agent pt-hub-n4xznj・pane w16T:p3"/)
})

test('側欄暱稱：沒有 run 只剩暱稱，不帶 herdr 名（沒選取時照舊沒有 tooltip）', () => {
  assert.doesNotMatch(row(null), /herdr|title=/)
  assert.doesNotMatch(row(null, true), /herdr/)
})

test('標題列 herdr 名：頭可截、尾 6 碼另一段（CSS 只截頭）', () => {
  const html = renderToStaticMarkup(<HerdrAgentName agent="pt-hub-n4xznj" />)
  assert.match(html, /<span class="pane-agent-head">pt-hub-<\/span><span class="pane-agent-tail">n4xznj<\/span>/)
})
