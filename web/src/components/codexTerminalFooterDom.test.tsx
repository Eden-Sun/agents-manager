import test, { before, after, afterEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, TerminalSnapshot } from '../api/types'
import fixture from '../testing/fixtures/codex-1664-terminal.json'
import { TerminalTab } from './TerminalTab'
import { BlockedPanel } from './BlockedPanel'
import { BlockedModal } from './BlockedModal'
import { StatusLineBar } from './ChatPanel'

before(setupDom)
afterEach(async () => { await unmountAll(); resetStoreForTest() })
after(teardownDom)

for (const view of ['terminal', 'panel', 'modal'] as const) {
  test(`${view}: 終端畫面保留 footer 行（不另畫一條），狀態列仍只有對話面板那條（#1680）`, async () => {
    let deliver!: (snap: TerminalSnapshot) => void
    useStore.setState({
      selectedBotId: 'b1', bots: [{ id: 'b1', kind: 'codex' } as Bot],
      readTerminal: () => new Promise((resolve) => { deliver = resolve }),
    })
    await mount(<>
      <StatusLineBar botId="b1" status={null} text={null} />
      {view === 'terminal' ? <TerminalTab botId="b1" /> : view === 'panel'
        ? <BlockedPanel botId="b1" onExpand={() => {}} />
        : <BlockedModal botId="b1" onClose={() => {}} />}
    </>)
    await act(async () => { deliver(fixture as TerminalSnapshot) })
    const body = document.querySelector('pre.term')?.textContent ?? ''
    assert.ok(body.includes('Working (12s'), body)
    assert.ok(body.includes('GPT-6-Astra') && body.includes('? for shortcuts') && body.includes('f2 to view'), body)
    assert.equal(document.querySelectorAll('.statusline-bar').length, 1)
    assert.equal(document.querySelector('.term-pane .statusline-bar, .blocked-modal .statusline-bar'), null)
    const bar = document.querySelector('.statusline-bar')?.textContent ?? ''
    assert.ok(bar.includes('~/project/agents-manager') && bar.includes('1 warning'), bar)
  })
}

test('狀態列不沿用別顆 bot 或前一個 run 的 warning', async () => {
  useStore.setState({ terminalFooter: { botId: 'old', runId: null, footer: { modelLine: null, warnings: 6 } } })
  await mount(<StatusLineBar botId="new" status={null} text={null} />)
  assert.equal(document.querySelector('.statusline-bar'), null)
  await act(() => useStore.setState({ terminalFooter: { botId: 'new', runId: 'previous-run', footer: { modelLine: null, warnings: 6 } } }))
  assert.equal(document.querySelector('.statusline-bar'), null)
})

test('手機終端不另畫 footer 條；warning 留在畫面裡，刷新沒有 warning 時 statusline 不留舊數字', async () => {
  const original = window.matchMedia
  window.matchMedia = ((q: string) => ({ matches: q.includes('max-width: 640px'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  try {
    let deliver!: (s: TerminalSnapshot) => void
    useStore.setState({ bots: [{ id: 'b1', kind: 'codex' } as Bot], readTerminal: () => new Promise((resolve) => { deliver = resolve }) })
    await mount(<TerminalTab botId="b1" />)
    await act(async () => { deliver(fixture as TerminalSnapshot) })
    assert.equal(document.querySelector('.term-pane .statusline-bar'), null)
    assert.ok(document.querySelector('pre.term')?.textContent?.includes('1 warning'))
    await act(async () => { (document.querySelector('.term-bar button') as HTMLButtonElement).click() })
    await act(async () => { deliver({ ...fixture, text: '正文\nGPT-6-Astra high · ~/repo\n? for shortcuts' } as TerminalSnapshot) })
    assert.ok(!document.querySelector('.statusline-bar')?.textContent?.includes('warning'))
    assert.equal(document.querySelector('pre.term')?.textContent, '正文\nGPT-6-Astra high · ~/repo\n? for shortcuts')
  } finally {
    await unmountAll()
    window.matchMedia = original
  }
})

test('run 已換但新快照尚未回來，不把舊 snapshot 的 warning 改掛新 run', async () => {
  let deliver!: (s: TerminalSnapshot) => void
  useStore.setState({ bots: [{ id: 'b1', kind: 'codex' } as Bot], readTerminal: () => new Promise((resolve) => { deliver = resolve }) })
  await mount(<><StatusLineBar botId="b1" status={null} text={null} /><TerminalTab botId="b1" /></>)
  await act(async () => { deliver(fixture as TerminalSnapshot) })
  assert.ok(document.querySelector('.statusline-bar')?.textContent?.includes('1 warning'))
  await act(async () => { useStore.setState({ runs: { b1: { id: 'next-run' } as import('../api/types').Run } }) })
  assert.ok(!document.querySelector('.statusline-bar')?.textContent?.includes('warning'))
})


test('舊 run 的 HTTP 快照晚到時，不在新 run 顯示 warning', async () => {
  let deliver!: (s: TerminalSnapshot) => void
  useStore.setState({ bots: [{ id: 'b1', kind: 'codex' } as Bot], readTerminal: () => new Promise((resolve) => { deliver = resolve }) })
  await mount(<><StatusLineBar botId="b1" status={null} text={null} /><TerminalTab botId="b1" /></>)
  await act(async () => { useStore.setState({ runs: { b1: { id: 'next-run' } as import('../api/types').Run } }) })
  const { toTerminal } = await import('../api/normalize')
  await act(async () => { deliver(toTerminal({ ...fixture, run_id: 'previous-run' }, 'visible')) })
  assert.ok(!document.querySelector('.statusline-bar')?.textContent?.includes('warning'))
})
