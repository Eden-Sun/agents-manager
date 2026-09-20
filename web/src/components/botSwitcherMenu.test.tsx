import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { BotSwitcherMenu } from './BotSwitcher.tsx'
import type { Bot, Project } from '../api/types'

const project = { id: 'p1', label: 'proj' } as Project
const bots = [{ id: 'b1', name: 'one', kind: 'claude', project_id: 'p1' }, { id: 'b2', name: 'two', kind: 'codex', project_id: 'p1' }] as Bot[]

// listbox／option 承諾方向鍵導覽與 aria-activedescendant，這裡是 button 清單；改成 menu 才有 useMenuKeys 的鍵盤行為。
test('換 bot 選單：menu＋menuitemradio，目前那顆 aria-checked，項目 tabIndex=-1（由 useMenuKeys 管焦點）', () => {
  const html = renderToStaticMarkup(<BotSwitcherMenu groups={[{ project, bots }]} botId="b2" onPick={() => {}} />)
  assert.match(html, /role="menu"/)
  assert.equal((html.match(/role="menuitemradio"/g) ?? []).length, 2)
  assert.equal((html.match(/aria-checked="true"/g) ?? []).length, 1)
  assert.doesNotMatch(html, /role="(listbox|option)"/)
  assert.equal((html.match(/tabindex="-1"/g) ?? []).length, 2)
})
