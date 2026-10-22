import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { act, click, mount, setupDom, teardownDom, typeInto, unmountAll, until } from '../testing/domHarness'
import type { Bot, Run, TerminalSnapshot } from '../api/types.ts'
import { CodexAsyncQuestion } from './CodexAsyncQuestion.tsx'
import { BlockedChoices } from './BlockedChoices.tsx'
import { parseChoiceMenu } from '../lib/tuiChoices.ts'

const { resetStoreForTest, useStore } = await import('../store/store.ts')

before(setupDom)
afterEach(unmountAll)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

// 2026-10-21 真機（codex 0.163.0，隔離 tmux session）：issue #1879。
const fx = (name: string) => readFileSync(new URL(`../lib/__fixtures__/${name}`, import.meta.url), 'utf8')
const COLLAPSED = fx('codex-0.163-async-collapsed.txt')
const EXPANDED = fx('codex-0.163-async-expanded.txt')
const FREE_TEXT = fx('codex-0.163-async-free-text.txt')

const bot = (kind: 'codex' | 'claude') => ({ id: 'b1', name: 'ci', project_id: 'p1', kind }) as unknown as Bot
const buttonByText = (label: string) =>
  [...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim().startsWith(label))

interface Rig {
  keys: string[][]
  texts: { text: string; enter: boolean }[]
  /** 每次送出時帶的固定 Run（`pinRunId`）。 */
  pins: (string | null | undefined)[]
  /** 之後每次重讀畫面回的字。 */
  screen: { text: string; runId?: string }
}

/** 畫面由測試控制；`onKeys`／`onText` 讓測試在送出後換畫面（模擬 codex 的反應）。 */
async function setup(
  kind: 'codex' | 'claude',
  screen: string,
  hooks: { onKeys?: (rig: Rig, keys: string[]) => void; onText?: (rig: Rig, text: string) => void; onRead?: (rig: Rig, n: number) => void } = {},
): Promise<Rig> {
  resetStoreForTest()
  const rig: Rig = { keys: [], texts: [], pins: [], screen: { text: screen } }
  let reads = 0
  await act(() =>
    useStore.setState({
      bots: [bot(kind)],
      runs: { b1: { id: 'r1' } as Run },
      notices: [],
      readTerminal: async () => {
        hooks.onRead?.(rig, ++reads)
        return { text: rig.screen.text, ...(rig.screen.runId ? { run_id: rig.screen.runId } : {}) } as TerminalSnapshot
      },
      sendKeys: async (_id: string, keys: string[], pin?: string | null) => {
        rig.keys.push(keys)
        rig.pins.push(pin)
        hooks.onKeys?.(rig, keys)
      },
      sendText: async (_id: string, text: string, enter: boolean, _record?: boolean, pin?: string | null) => {
        rig.texts.push({ text, enter })
        rig.pins.push(pin)
        hooks.onText?.(rig, text)
        return true
      },
    } as never),
  )
  return rig
}

test('收合的提問：按「展開作答」只送一下 shift+←，畫面展開後才算成功', async () => {
  const rig = await setup('codex', COLLAPSED, { onKeys: (r, keys) => {
    if (keys[0] === 'shift+left') r.screen.text = EXPANDED
  } })
  const root = await mount(<CodexAsyncQuestion botId="b1" text={COLLAPSED} />)
  assert.ok(root.textContent?.includes('1 題非同步提問'))
  await click(buttonByText('展開作答')!)
  await until(() => rig.keys.length === 1 && !root.querySelector('[role="status"]'), '送出 shift+←')
  assert.deepEqual(rig.keys, [['shift+left']], '只送展開，不替使用者選任何答案')
  assert.deepEqual(rig.texts, [])
})

test('macOS glyphs: expand uses shift+left, then directions and Enter are separate pinned steps', async () => {
  const collapsed = fx('codex-0.163.0-macos-collapsed.txt')
  const expanded = fx('codex-0.163.0-macos-expanded.txt')
  const moved = expanded.replace('› 1. 蘋果', '  1. 蘋果').replace('  2. 香蕉', '› 2. 香蕉')
  const rig = await setup('codex', collapsed, { onKeys: (r, keys) => {
    if (keys[0] === 'shift+left') r.screen.text = expanded
    if (keys[0] === 'down') r.screen.text = moved
  } })
  const root = await mount(<CodexAsyncQuestion botId="b1" text={collapsed} />)
  assert.ok(root.textContent?.includes('1 題非同步提問'))
  await click(buttonByText('展開作答')!)
  await until(() => rig.keys.length === 1 && !root.querySelector('[role="status"]'), 'macOS expand')
  assert.deepEqual(rig.keys, [['shift+left']])
  await unmountAll()
  const menu = parseChoiceMenu(expanded)
  assert.ok(menu?.asyncQueue)
  const choices = await mount(<BlockedChoices botId="b1" menu={menu} />)
  await click(choices.querySelectorAll<HTMLButtonElement>('.bc-item')[1])
  await until(() => rig.keys.some((keys) => keys[0] === 'enter'), 'macOS answer after cursor confirmation')
  assert.deepEqual(rig.keys, [['shift+left'], ['down'], ['enter']], 'never send display glyphs or combine movement with submit')
  assert.deepEqual(rig.pins, ['r1', 'r1', 'r1'])
  assert.deepEqual(rig.texts, [])
})

test('送鍵前重讀畫面：畫面已經不是收合的提問就什麼都不送', async () => {
  const rig = await setup('codex', EXPANDED)
  const root = await mount(<CodexAsyncQuestion botId="b1" text={COLLAPSED} />)
  await click(buttonByText('展開作答')!)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現')
  assert.deepEqual(rig.keys, [])
  assert.match(root.querySelector('[role="status"]')!.textContent ?? '', /沒有送出 shift\+←/)
})

test('按了 shift+← 但畫面沒展開：照實說，不再補任何鍵', async () => {
  const rig = await setup('codex', COLLAPSED)
  const root = await mount(<CodexAsyncQuestion botId="b1" text={COLLAPSED} />)
  await click(buttonByText('展開作答')!)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現', 10_000)
  assert.deepEqual(rig.keys, [['shift+left']])
  assert.match(root.querySelector('[role="status"]')!.textContent ?? '', /沒有展開/)
})

test('編號選單展開後不畫任何東西（交給 BlockedChoices）；不是 codex 也不畫', async () => {
  await setup('codex', EXPANDED)
  assert.equal((await mount(<CodexAsyncQuestion botId="b1" text={EXPANDED} />)).innerHTML, '')
  await unmountAll()
  await setup('claude', COLLAPSED)
  assert.equal((await mount(<CodexAsyncQuestion botId="b1" text={COLLAPSED} />)).innerHTML, '')
})


const NAME_Q = [{ question: 'Your name?', header: '', multiSelect: false, options: [] }]
const typedScreen = (t: string) => FREE_TEXT.replace('Type your answer', t)
/** 已切回主輸入框：提問面板沒了，字落在別處。 */
const MAIN_PROMPT = '• Pick a size?\n  Your name?\n› Bob\n  GPT-6.1-Sol default · ~/x\n'
const answerWith = (shown: string) => (r: Rig) => {
  r.screen.text = typedScreen(shown)
}

test('自由輸入題：輸入框是空的才貼字；完整內容等於答案才按 Enter；送鍵都帶固定的 Run', async () => {
  const rig = await setup('codex', FREE_TEXT, { onText: answerWith('Bob') })
  const root = await mount(<CodexAsyncQuestion botId="b1" text={FREE_TEXT} questions={NAME_Q} />)
  await typeInto(root.querySelector('textarea')!, 'Bob')
  await click(buttonByText('送出答案')!)
  await until(() => rig.keys.length === 1, '送出 Enter')
  assert.deepEqual(rig.texts, [{ text: 'Bob', enter: false }])
  assert.deepEqual(rig.keys, [['enter']])
  assert.deepEqual(rig.pins, ['r1', 'r1'], '貼字與 Enter 都帶這次作答固定的 Run')
})

test('自由輸入題：字沒有出現在畫面上就不按 Enter', async () => {
  const rig = await setup('codex', FREE_TEXT)
  const root = await mount(<CodexAsyncQuestion botId="b1" text={FREE_TEXT} questions={NAME_Q} />)
  await typeInto(root.querySelector('textarea')!, 'Bob')
  await click(buttonByText('送出答案')!)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現', 10_000)
  assert.deepEqual(rig.keys, [], '沒按 Enter')
  assert.match(root.querySelector('[role="status"]')!.textContent ?? '', /不等於要送的答案/)
})

test('反例：貼字後使用者切回主輸入框（字出現在別處）：不按 Enter', async () => {
  const rig = await setup('codex', FREE_TEXT, { onText: (r) => (r.screen.text = MAIN_PROMPT) })
  const root = await mount(<CodexAsyncQuestion botId="b1" text={FREE_TEXT} questions={NAME_Q} />)
  await typeInto(root.querySelector('textarea')!, 'Bob')
  await click(buttonByText('送出答案')!)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現', 10_000)
  assert.deepEqual(rig.keys, [])
})

test('反例：最後一次確認時畫面又切走了（貼字確認之後、Enter 之前）：不按 Enter', async () => {
  let afterPaste = 0
  const rig = await setup('codex', FREE_TEXT, {
    onText: (r) => {
      r.screen.text = typedScreen('Bob')
      afterPaste = 1
    },
    // 貼字後第 1 讀（對帳）正常，第 2 讀（送 Enter 前的最後確認）換成主輸入框。
    onRead: (r) => {
      if (afterPaste && ++afterPaste >= 3) r.screen.text = MAIN_PROMPT
    },
  })
  const root = await mount(<CodexAsyncQuestion botId="b1" text={FREE_TEXT} questions={NAME_Q} />)
  await typeInto(root.querySelector('textarea')!, 'Bob')
  await click(buttonByText('送出答案')!)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現', 10_000)
  assert.deepEqual(rig.keys, [])
  assert.match(root.querySelector('[role="status"]')!.textContent ?? '', /又變了/)
})

test('反例：面板上是別的題目：不貼字、不按 Enter', async () => {
  const rig = await setup('codex', FREE_TEXT)
  const other = [{ question: '另一題？', header: '', multiSelect: false, options: [] }]
  const root = await mount(<CodexAsyncQuestion botId="b1" text={FREE_TEXT} questions={other} />)
  await typeInto(root.querySelector('textarea')!, 'Bob')
  await click(buttonByText('送出答案')!)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現')
  assert.deepEqual(rig.texts, [])
  assert.deepEqual(rig.keys, [])
})

// 複審 P2：舊題「Name?」不能對上新題「Full Name?」（子字串比對會）。
test('反例：面板上是「Full Name?」、預期的是「Name?」：不貼字、不按 Enter', async () => {
  const panel = FREE_TEXT.replace('Your name?\n  Type', 'Full Name?\n  Type')
  const rig = await setup('codex', panel)
  const root = await mount(<CodexAsyncQuestion botId="b1" text={panel} questions={[{ ...NAME_Q[0], question: 'Name?' }]} />)
  await typeInto(root.querySelector('textarea')!, 'Bob')
  await click(buttonByText('送出答案')!)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現')
  assert.deepEqual(rig.texts, [])
  assert.deepEqual(rig.keys, [])
})

// 複審 P3：輸入框原本有 Alice、貼 Bob 變成 AliceBob，只比後綴會通過並提交。
test('反例：輸入框裡原本就有字（Alice）：不在上面疊字', async () => {
  const withAlice = typedScreen('Alice')
  const rig = await setup('codex', withAlice)
  const root = await mount(<CodexAsyncQuestion botId="b1" text={withAlice} questions={NAME_Q} />)
  await typeInto(root.querySelector('textarea')!, 'Bob')
  await click(buttonByText('送出答案')!)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現')
  assert.deepEqual(rig.texts, [])
  assert.deepEqual(rig.keys, [])
})

test('反例：貼字前是空的、貼完卻變成 AliceBob（後綴是答案但不是完整內容）：不按 Enter', async () => {
  const rig = await setup('codex', FREE_TEXT, { onText: answerWith('AliceBob') })
  const root = await mount(<CodexAsyncQuestion botId="b1" text={FREE_TEXT} questions={NAME_Q} />)
  await typeInto(root.querySelector('textarea')!, 'Bob')
  await click(buttonByText('送出答案')!)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現', 10_000)
  assert.deepEqual(rig.keys, [])
})

// 複審 P1：讀畫面後 bot 重啟，文字與 Enter 不能送進新的 Run。
test('反例：貼字後 Run 換了（bot 重啟）：不按 Enter，也不走 fresh-run 重試', async () => {
  const rig = await setup('codex', FREE_TEXT, {
    onText: (r) => {
      r.screen.text = typedScreen('Bob')
      r.screen.runId = 'r2'
    },
  })
  const root = await mount(<CodexAsyncQuestion botId="b1" text={FREE_TEXT} questions={NAME_Q} />)
  await typeInto(root.querySelector('textarea')!, 'Bob')
  await click(buttonByText('送出答案')!)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現', 10_000)
  assert.deepEqual(rig.keys, [])
  assert.deepEqual(rig.pins, ['r1'], '只有貼字那一次，帶的是開始時固定的 Run')
  assert.match(root.querySelector('[role="status"]')!.textContent ?? '', /Run 已經換了/)
})

test('反例：展開後 store 裡的 Run 換了（bot 重啟）：展開那一鍵帶的是固定的舊 Run，之後放棄並說明', async () => {
  const rig = await setup('codex', COLLAPSED)
  // 第一讀固定在 r1；送出 shift+← 之後 store 顯示換成 r2。
  await act(() =>
    useStore.setState({
      readTerminal: async () => ({ text: COLLAPSED }) as TerminalSnapshot,
      sendKeys: async (_id: string, keys: string[], pin?: string | null) => {
        rig.keys.push(keys)
        rig.pins.push(pin)
        useStore.setState({ runs: { b1: { id: 'r2' } as Run } })
      },
    } as never),
  )
  const root = await mount(<CodexAsyncQuestion botId="b1" text={COLLAPSED} />)
  await click(buttonByText('展開作答')!)
  await until(() => Boolean(root.querySelector('[role="status"]')), '提示出現', 10_000)
  assert.deepEqual(rig.keys, [['shift+left']])
  assert.deepEqual(rig.pins, ['r1'], '送鍵帶的是開始時固定的 Run，不是之後的新 Run')
  assert.match(root.querySelector('[role="status"]')!.textContent ?? '', /Run 已經換了/)
})

test('反例：讀不到這題的原文（或有好幾題）：不給打字框，請使用者在終端作答', async () => {
  await setup('codex', FREE_TEXT)
  const none = await mount(<CodexAsyncQuestion botId="b1" text={FREE_TEXT} />)
  assert.equal(none.querySelector('textarea'), null)
  assert.match(none.textContent ?? '', /請直接在終端作答/)
  await unmountAll()
  const two = await mount(<CodexAsyncQuestion botId="b1" text={FREE_TEXT} questions={[...NAME_Q, ...NAME_Q]} />)
  assert.equal(two.querySelector('textarea'), null)
})
