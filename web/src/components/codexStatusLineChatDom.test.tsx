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

/** claude 那條：帳號／目錄、context、快取、版本；每個欄位是 `.sl-item`，不塞原文字串。 */
function items() {
  return [...document.querySelectorAll('.statusline-bar > .sl-item')].map((e) => ({
    k: e.getAttribute('data-k') ?? '', v: e.querySelector('.sl-v')?.textContent ?? '',
  }))
}

test('對話分頁沒掛終端：codex 跟 claude 同一種欄位（目錄、context 百分比），不塞 footer 原文，也沒有孤零零的 context 字樣', async () => {
  useStore.setState({ bots: [{ id: 'b1', kind: 'codex' } as Bot] })
  await mount(<StatusLineBar botId="b1" status={status} text={CONTEXT_LINE} />)
  const bar = document.querySelector('.statusline-bar')!
  assert.equal(bar.querySelector('.statusline-text'), null, '不畫原文字串')
  assert.ok(!(bar.textContent ?? '').includes('weekly'), bar.textContent ?? '')
  const got = items()
  assert.deepEqual(got.slice(0, 2).map((x) => x.k), ['目錄', 'context'])
  assert.equal(got[0].v, '~/project/hermes-agents/projects/rt', '目錄用 pane 上那個寫法')
  assert.ok(got[1].v.startsWith('18%'), got[1].v)
  for (const it of got) assert.ok(it.v.trim() !== '', `${it.k} 不能是空值（孤零零的標籤）`)
  assert.equal(bar.getAttribute('title'), CONTEXT_LINE, '原文留在 title')
})

test('對話分頁沒有 status 物件時，codex 的 footer 一樣拆成目錄＋context 欄位', async () => {
  useStore.setState({ bots: [{ id: 'b1', kind: 'codex' } as Bot] })
  await mount(<StatusLineBar botId="b1" status={null} text={CONTEXT_LINE} />)
  assert.equal(document.querySelector('.statusline-bar .statusline-text'), null)
  assert.deepEqual(items().map((x) => x.k), ['目錄', 'context'])
  assert.ok(items()[1].v.startsWith('18%'))
})

test('footer 沒有 Context 段時不畫 context 欄位；主題不畫', async () => {
  useStore.setState({ bots: [{ id: 'b1', kind: 'codex' } as Bot] })
  await mount(<StatusLineBar botId="b1" status={null} text={SHORTCUTS_LINE} />)
  assert.deepEqual(items(), [{ k: '目錄', v: '~/project/agm-lx1685' }])
})

test('終端分頁開著時，狀態列以終端解析的 footer 為準（含 warning），不再多畫一份 status_line', async () => {
  useStore.setState({
    bots: [{ id: 'b1', kind: 'codex' } as Bot],
    runs: { b1: { id: context.run_id } as Run },
    readTerminal: () => Promise.resolve(context as TerminalSnapshot),
  })
  await mount(<><StatusLineBar botId="b1" status={status} text={CONTEXT_LINE} /><TerminalTab botId="b1" /></>)
  await act(async () => { await new Promise((r) => setTimeout(r, 20)) })
  assert.equal(document.querySelectorAll('.statusline-bar').length, 1)
  assert.deepEqual(items().slice(0, 2).map((x) => x.k), ['目錄', 'context'])
  assert.equal(document.querySelector('.statusline-bar .statusline-text'), null)
})

test('非 codex 的 status_line 不當成 codex footer 解析', async () => {
  useStore.setState({ bots: [{ id: 'b1', kind: 'claude' } as Bot] })
  await mount(<StatusLineBar botId="b1" status={status} text={CONTEXT_LINE} />)
  const bar = document.querySelector('.statusline-bar')?.textContent ?? ''
  assert.ok(!bar.includes('weekly 78% left'), bar)
  assert.equal(document.querySelector('.statusline-bar .statusline-text'), null)
})
