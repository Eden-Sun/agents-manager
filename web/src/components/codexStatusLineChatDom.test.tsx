/**
 * #1755：codex 的 footer（模型、目錄、Context／額度那串）不只在終端分頁有。
 * 終端畫面本身照 pane 原樣留著 footer（用線上 `GET /api/bots/{id}/terminal` 的真回應當 fixture，桌機與手機寬度各驗一次）；
 * 對話分頁沒有掛終端時，狀態列改用 daemon 寫進 `run.status_line` 的同一行，不再只剩 context 百分比。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, Run, StatusInfo, TerminalSnapshot } from '../api/types'
import context from '../testing/fixtures/codex-1755-terminal-context.json'
import shortcuts from '../testing/fixtures/codex-1755-terminal-shortcuts.json'
import { TerminalTab } from './TerminalTab'
import { StatusLineBar } from './ChatPanel'

before(setupDom)
afterEach(async () => { await unmountAll(); resetStoreForTest() })
after(teardownDom)

const CONTEXT_LINE = 'GPT-6.1-Sol low · ~/project/hermes-agents/projects/rt · Context 18% used · weekly 78% left'
const SHORTCUTS_LINE = 'GPT-6.1-Sol low · ~/project/agm-lx1685 · 接手並完成 issue #1685'

const status = {
  account_email: null, account_warning: null, model_name: 'GPT-6.1-Sol', model_id: 'gpt-6.1-sol', effort: 'low', thinking: false, fast_mode: false,
  context_used_pct: 18, context_used_tokens: null, context_size: null, five_hour_pct: null, five_hour_resets_at: null,
  seven_day_pct: null, seven_day_resets_at: null, cost_usd: null, cwd: '/home/ubuntu/project/hermes-agents/projects/rt', version: null, session_name: null,
} as StatusInfo

function phoneWidth(on: boolean) {
  const original = window.matchMedia
  window.matchMedia = ((q: string) => ({ matches: on && q.includes('max-width: 640px'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  return () => { window.matchMedia = original }
}

for (const phone of [false, true]) {
  for (const [name, fixture, line] of [['context', context, CONTEXT_LINE], ['shortcuts', shortcuts, SHORTCUTS_LINE]] as const) {
    test(`終端分頁（${phone ? '手機' : '桌機'}寬度，${name} footer）：pane 最底下那行 statusline 照原樣在畫面裡`, async () => {
      const restore = phoneWidth(phone)
      try {
        useStore.setState({
          bots: [{ id: 'b1', kind: 'codex' } as Bot],
          readTerminal: () => Promise.resolve(fixture as TerminalSnapshot),
        })
        await mount(<TerminalTab botId="b1" />)
        await act(async () => {})
        const rows = (document.querySelector('pre.term')?.textContent ?? '').split('\n').map((r) => r.trim()).filter(Boolean)
        const at = rows.indexOf(line)
        assert.ok(at >= 0, `footer 行要在終端畫面裡：${rows.slice(-4).join(' | ')}`)
        assert.equal(rows.filter((r) => r === line).length, 1, '只畫一次')
        assert.equal(document.querySelector('.term-pane .statusline-bar'), null, '終端分頁不另畫一條')
      } finally {
        restore()
      }
    })
  }
}

test('對話分頁沒掛終端：狀態列用 run.status_line 畫出 codex 的模型列，不重複 context 百分比', async () => {
  useStore.setState({ bots: [{ id: 'b1', kind: 'codex' } as Bot] })
  await mount(<StatusLineBar botId="b1" status={status} text={CONTEXT_LINE} />)
  const bar = document.querySelector('.statusline-bar')?.textContent ?? ''
  assert.ok(bar.includes(CONTEXT_LINE), bar)
  assert.equal(bar.match(/18%/g)?.length, 1, `context 只畫一次：${bar}`)
  assert.equal(bar.match(/projects\/rt/g)?.length, 1, `目錄只畫一次：${bar}`)
})

test('對話分頁沒有 status 物件時一樣畫 run.status_line 的 codex 模型列', async () => {
  useStore.setState({ bots: [{ id: 'b1', kind: 'codex' } as Bot] })
  await mount(<StatusLineBar botId="b1" status={null} text={SHORTCUTS_LINE} />)
  assert.ok((document.querySelector('.statusline-bar')?.textContent ?? '').includes(SHORTCUTS_LINE))
})

test('終端分頁開著時，狀態列以終端解析的 footer 為準（含 warning），不再多畫一份 status_line', async () => {
  useStore.setState({
    bots: [{ id: 'b1', kind: 'codex' } as Bot],
    runs: { b1: { id: context.run_id } as Run },
    readTerminal: () => Promise.resolve(context as TerminalSnapshot),
  })
  await mount(<><StatusLineBar botId="b1" status={status} text={CONTEXT_LINE} /><TerminalTab botId="b1" /></>)
  await act(async () => { await new Promise((r) => setTimeout(r, 20)) })
  const bar = document.querySelector('.statusline-bar')?.textContent ?? ''
  assert.equal(bar.split('Context 18% used').length - 1, 1, bar)
})

test('非 codex 的 status_line 不當成 codex footer 解析', async () => {
  useStore.setState({ bots: [{ id: 'b1', kind: 'claude' } as Bot] })
  await mount(<StatusLineBar botId="b1" status={status} text={CONTEXT_LINE} />)
  const bar = document.querySelector('.statusline-bar')?.textContent ?? ''
  assert.ok(!bar.includes('weekly 78% left'), bar)
})
