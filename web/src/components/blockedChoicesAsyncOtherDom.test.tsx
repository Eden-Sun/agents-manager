import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { act, click, mount, setupDom, teardownDom, typeInto, unmountAll, until } from '../testing/domHarness'
import type { Run, TerminalSnapshot } from '../api/types.ts'
import { parseChoiceMenu } from '../lib/tuiChoices.ts'
import { BlockedChoices } from './BlockedChoices.tsx'
import { skipTimerDelays } from '../testing/skipTimerDelays'

const { resetStoreForTest, useStore } = await import('../store/store.ts')

// 元件送鍵後固定等 STEP（260ms）讓畫面反應；rig 的畫面是立刻換好的，不必睡過去。
let fast: ReturnType<typeof skipTimerDelays>
before(async () => {
  await setupDom()
  fast = skipTimerDelays([260])
})
afterEach(unmountAll)
after(async () => {
  fast.restore()
  resetStoreForTest()
  await teardownDom()
})

// codex 0.163.0 真機的展開畫面（issue #1879）；游標與 Other 列的字由 `screen()` 換。
const BASE = readFileSync(new URL('../lib/__fixtures__/codex-0.163-async-expanded.txt', import.meta.url), 'utf8')
const screen = (o: { cursor?: number; other?: string; question?: string } = {}) => {
  const rows = ['red', 'blue', 'green', o.other ?? 'Other']
  const body = rows.map((r, i) => `  ${i === (o.cursor ?? 0) ? '›' : ' '} ${i + 1}. ${r}`).join('\n')
  return BASE.replace(/ {2}Which color\?[\s\S]*?\n( {2}enter submit)/, `  ${o.question ?? 'Which color?'}\n${body}\n$1`)
}
const MAIN_PROMPT = '• Which color?\n  • red\n› \n  GPT-6.1-Sol default · ~/x\n'

interface Rig {
  keys: string[][]
  texts: string[]
  text: string
  /** 每次送出帶的固定 Run（`pinRunId`）。 */
  pins: (string | null | undefined)[]
  runId?: string
}

/** 畫面由測試控制：`react(rig, event)` 在 down／貼字／讀取時換畫面。 */
async function setup(opts: { onKeys?: (r: Rig, keys: string[]) => void; onText?: (r: Rig, t: string) => void; onRead?: (r: Rig, n: number) => void }): Promise<Rig> {
  resetStoreForTest()
  const rig: Rig = { keys: [], texts: [], text: screen(), pins: [] }
  let reads = 0
  await act(() =>
    useStore.setState({
      bots: [{ id: 'b1', kind: 'codex' }],
      runs: { b1: { id: 'r1' } as Run },
      notices: [],
      readTerminal: async () => {
        opts.onRead?.(rig, ++reads)
        return { text: rig.text, ...(rig.runId ? { run_id: rig.runId } : {}) } as TerminalSnapshot
      },
      sendKeys: async (_id: string, keys: string[], pin?: string | null) => {
        rig.keys.push(keys)
        rig.pins.push(pin)
        opts.onKeys?.(rig, keys)
      },
      sendText: async (_id: string, t: string, _enter?: boolean, _record?: boolean, pin?: string | null) => {
        rig.texts.push(t)
        rig.pins.push(pin)
        opts.onText?.(rig, t)
        return true
      },
    } as never),
  )
  return rig
}

async function openOther(): Promise<HTMLElement> {
  const menu = parseChoiceMenu(screen())!
  const root = await mount(<BlockedChoices botId="b1" menu={menu} />)
  await click(root.querySelectorAll<HTMLButtonElement>('.bc-item')[3])
  await typeInto(root.querySelector('textarea')!, '紫色')
  return root
}
const send = (root: HTMLElement) => click(root.querySelector<HTMLButtonElement>('.bc-type-send')!)
const walk = (r: Rig, keys: string[]) => {
  const cursor = keys.filter((k) => k === 'down').length
  if (cursor) r.text = screen({ cursor })
}

test('Other：同一題、游標在 Other、字在這一列、提問面板展開中才按 Enter', async () => {
  const rig = await setup({ onKeys: walk, onText: (r, t) => (r.text = screen({ cursor: 3, other: t })) })
  const root = await openOther()
  await send(root)
  await until(() => rig.keys.some((k) => k[0] === 'enter'), '送出 Enter')
  assert.deepEqual(rig.texts, ['紫色'])
})

// 複審反例（P1）：只確認同索引那列出現文字不夠。
test('反例：貼字後換成下一題（Other 列同樣出現那段字）：不按 Enter', async () => {
  const rig = await setup({
    onKeys: walk,
    onText: (r, t) => (r.text = screen({ cursor: 3, other: t, question: 'Another question?' })),
  })
  const root = await openOther()
  await send(root)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現', 10_000)
  assert.ok(!rig.keys.some((k) => k[0] === 'enter'))
})

test('反例：貼字後游標不在 Other 列：不按 Enter', async () => {
  const rig = await setup({ onKeys: walk, onText: (r, t) => (r.text = screen({ cursor: 1, other: t })) })
  const root = await openOther()
  await send(root)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現', 10_000)
  assert.ok(!rig.keys.some((k) => k[0] === 'enter'))
})

test('反例：游標走到 Other 後、貼字前就切回主輸入框：什麼字都不貼', async () => {
  const rig = await setup({ onKeys: (r, keys) => (keys.includes('down') ? (r.text = screen({ cursor: 3 })) : undefined) })
  const root = await openOther()
  // walkTo 最後一次確認後、貼字前的重讀換成主輸入框。
  let armed = false
  await act(() =>
    useStore.setState({
      readTerminal: async () => {
        if (armed) rig.text = MAIN_PROMPT
        if (rig.keys.length) armed = true
        return { text: rig.text } as TerminalSnapshot
      },
    } as never),
  )
  await send(root)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現', 10_000)
  assert.deepEqual(rig.texts, [])
  assert.ok(!rig.keys.some((k) => k[0] === 'enter'))
})

test('反例：最後一讀時提問面板已經不在（切回主輸入框）：不按 Enter', async () => {
  let afterPaste = 0
  const rig = await setup({
    onKeys: walk,
    onText: (r, t) => {
      r.text = screen({ cursor: 3, other: t })
      afterPaste = 1
    },
    onRead: (r) => {
      // 貼字後第 1 讀（字出現的對帳）正常，第 2 讀（送 Enter 前的最後確認）換成主輸入框。
      if (afterPaste && ++afterPaste >= 3) r.text = MAIN_PROMPT
    },
  })
  const root = await openOther()
  await send(root)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現', 10_000)
  assert.ok(!rig.keys.some((k) => k[0] === 'enter'))
})

// 複審：單選也不能把方向鍵與 Enter 一批送出去——移動期間可能換題，Enter 會答到別題。
async function openAndPick(index: number): Promise<HTMLElement> {
  const root = await mount(<BlockedChoices botId="b1" menu={parseChoiceMenu(screen())!} />)
  await click(root.querySelectorAll<HTMLButtonElement>('.bc-item')[index])
  return root
}

test('選項單選：先只送方向鍵，重讀確認同題且游標在目標，才另外送 Enter', async () => {
  const rig = await setup({ onKeys: (r, keys) => (keys.includes('down') ? (r.text = screen({ cursor: 1 })) : undefined) })
  await openAndPick(1)
  await until(() => rig.keys.some((k) => k[0] === 'enter'), '送出 Enter')
  assert.deepEqual(rig.keys, [['down'], ['enter']], '方向鍵與 Enter 不同批')
})

test('反例：移動期間換題（新題的游標剛好也在第 2 列）：不按 Enter', async () => {
  const rig = await setup({ onKeys: (r, keys) => (keys.includes('down') ? (r.text = screen({ cursor: 1, question: 'Another question?' })) : undefined) })
  const root = await openAndPick(1)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現', 10_000)
  assert.deepEqual(rig.keys, [['down']])
  assert.match(root.querySelector('[role="status"]')!.textContent ?? '', /沒有按 Enter/)
})

test('反例：移動後游標不在目標選項：不按 Enter', async () => {
  const rig = await setup({ onKeys: (r, keys) => (keys.includes('down') ? (r.text = screen({ cursor: 2 })) : undefined) })
  const root = await openAndPick(1)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現', 10_000)
  assert.deepEqual(rig.keys, [['down']])
})

test('反例：移動後切回主輸入框：不按 Enter', async () => {
  const rig = await setup({ onKeys: (r, keys) => (keys.includes('down') ? (r.text = MAIN_PROMPT) : undefined) })
  const root = await openAndPick(1)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現', 10_000)
  assert.deepEqual(rig.keys, [['down']])
})

// 複審：同題、游標在目標，但目標選項被改了字（其他列不變）——使用者點的不是現在這個內容。
test('反例：移動後目標選項的標題變了（同題、游標在目標、其他列不變）：不按 Enter', async () => {
  const changed = screen({ cursor: 1 }).replace('2. blue', '2. 完全不同的選項')
  const rig = await setup({ onKeys: (r, keys) => (keys.includes('down') ? (r.text = changed) : undefined) })
  const root = await openAndPick(1)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現', 10_000)
  assert.deepEqual(rig.keys, [['down']])
})

// 複審 P3：貼字後那一列必須完整等於答案（不是只出現那段字）。
test('反例：貼字後那一列變成「舊字＋答案」（後綴是答案但不是完整內容）：不按 Enter', async () => {
  const rig = await setup({ onKeys: walk, onText: (r, t) => (r.text = screen({ cursor: 3, other: `舊的${t}` })) })
  const root = await openOther()
  await send(root)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現', 10_000)
  assert.ok(!rig.keys.some((k) => k[0] === 'enter'))
})

// 複審 P1：作答固定在同一個 Run，送鍵都帶 id；Run 換了就放棄。
test('單選與 Other：每一次送出都帶這次作答固定的 Run', async () => {
  const pick = await setup({ onKeys: (r, keys) => (keys.includes('down') ? (r.text = screen({ cursor: 1 })) : undefined) })
  await openAndPick(1)
  await until(() => pick.keys.some((k) => k[0] === 'enter'), '送出 Enter')
  assert.deepEqual(pick.pins, ['r1', 'r1'])
  await unmountAll()

  const other = await setup({ onKeys: walk, onText: (r, t) => (r.text = screen({ cursor: 3, other: t })) })
  const root = await openOther()
  await send(root)
  await until(() => other.keys.some((k) => k[0] === 'enter'), '送出 Enter')
  assert.ok(other.pins.length >= 3 && other.pins.every((p) => p === 'r1'))
})

test('反例：移動後 Run 換了（bot 重啟）：不按 Enter、不重試新 Run', async () => {
  const rig = await setup({
    onKeys: (r, keys) => {
      if (keys.includes('down')) {
        r.text = screen({ cursor: 1 })
        r.runId = 'r2'
      }
    },
  })
  const root = await openAndPick(1)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現', 10_000)
  assert.deepEqual(rig.keys, [['down']])
  assert.deepEqual(rig.pins, ['r1'])
  assert.match(root.querySelector('[role="status"]')!.textContent ?? '', /Run 已經換了/)
})

test('反例：貼字後 Run 換了：不按 Enter', async () => {
  const rig = await setup({
    onKeys: walk,
    onText: (r, t) => {
      r.text = screen({ cursor: 3, other: t })
      r.runId = 'r2'
    },
  })
  const root = await openOther()
  await send(root)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現', 10_000)
  assert.ok(!rig.keys.some((k) => k[0] === 'enter'))
})
