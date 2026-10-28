import test, { before, after, afterEach } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, TerminalSnapshot } from '../api/types'
import { BlockedModal } from './BlockedModal'
import { BlockedPanel } from './BlockedPanel'

before(setupDom)
const originalFetch = globalThis.fetch
afterEach(async () => { await unmountAll(); globalThis.fetch = originalFetch; resetStoreForTest() })
after(teardownDom)
const fixture = (name: string) => readFileSync(new URL(`../lib/__fixtures__/${name}`, import.meta.url), 'utf8')
const collapsed = fixture('codex-0.163-async-collapsed.txt')
const freeText = fixture('codex-0.163-async-free-text.txt')
const update = 'Update available! 0.154.0 -> 0.155.1\n> 1. Update now\n  2. Skip\n  3. Skip until next version\n'
const raw = () => document.querySelector('pre.blocked-modal-term, pre.blocked-term')
const toggle = () => [...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.includes('終端原文'))
const flush = () => act(async () => { for (let i = 0; i < 25; i++) await Promise.resolve() })

async function open(panel: boolean, text: string, options: string[], kind = 'codex', pending = 'ready') {
  const keys: string[][] = []
  globalThis.fetch = (async () => {
    if (pending === 'loading') return new Promise<Response>(() => {})
    if (pending === 'unavailable') throw new Error('synthetic pending read failure')
    return new Response(JSON.stringify({ questions: [{ header: '顏色', question: '選哪個顏色？', multiSelect: false, options: options.map((label) => ({ label, description: '' })) }] }))
  }) as typeof fetch
  useStore.setState({
    bots: [{ id: 'b1', kind } as Bot],
    readTerminal: async () => ({ text, columns: 213, rows: 55, revision: 1 }) as TerminalSnapshot,
    sendKeys: async (_id: string, sent: string[]) => { keys.push(sent) },
  })
  await mount(panel ? <BlockedPanel botId="b1" /> : <BlockedModal botId="b1" onClose={() => {}} />)
  await flush()
  return keys
}

for (const panel of [false, true]) {
  const label = panel ? 'Panel' : 'Modal'
  for (const [name, text] of [['collapsed async', collapsed], ['update menu', update]]) {
    test(`${label}: ${name} options hide raw; toggle opens and closes without sending keys`, async () => {
      const keys = await open(panel, text, ['紅', '藍'])
      assert.equal(Boolean(raw()), false)
      assert.ok(toggle(), 'raw remains available with one click')
      await act(() => toggle()!.click())
      assert.ok(raw())
      assert.ok(raw()!.textContent?.includes(text.split('\n')[0]))
      await act(() => toggle()!.click())
      assert.equal(Boolean(raw()), false)
      assert.deepEqual(keys, [])
      assert.ok(document.querySelector('.blocked-modal-update'), 'dedicated controls remain')
    })
  }
  for (const [name, text, options, kind, pending] of [
    ['no options', collapsed, [], 'codex', 'ready'],
    ['free text', freeText, ['紅'], 'codex', 'ready'],
    ['loading', collapsed, ['紅'], 'codex', 'loading'],
    ['unavailable', collapsed, ['紅'], 'codex', 'unavailable'],
    ['claude', collapsed, ['紅'], 'claude', 'ready'],
    ['ordinary terminal', 'ordinary terminal history', ['紅'], 'codex', 'ready'],
    ['noninteractive update', 'Update available! v0.155.1', ['紅'], 'codex', 'ready'],
  ] as const) {
    test(`${label}: ${name} retains raw`, async () => {
      await open(panel, text, [...options], kind, pending)
      assert.ok(raw())
    })
  }
}
