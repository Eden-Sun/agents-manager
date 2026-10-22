import test, { before, after, afterEach } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { act, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { BlockedDraft } from '../components/BlockedDraft'
import { parseChoiceMenu } from './tuiChoices'
import type { TerminalSnapshot } from '../api/types'

const { resetStoreForTest, useStore } = await import('../store/store')
before(setupDom)
afterEach(unmountAll)
after(async () => { resetStoreForTest(); await teardownDom() })

const FULL = readFileSync(new URL('./__fixtures__/claude-repo-multiselect-1887.txt', import.meta.url), 'utf8')
const SHORT = FULL.slice(FULL.indexOf('←  ☐ Repo'))

test('#1887 手機草稿從短快照載入，完整快照重讀後仍能送出全部答案', async () => {
  resetStoreForTest()
  const width = window.innerWidth
  Object.defineProperty(window, 'innerWidth', { configurable: true, value: 390 })
  let cursor = 0
  let checked = false
  let stage = 'question'
  const sent: string[] = []
  const screen = () => {
    if (stage === 'done') return 'Working…'
    if (stage === 'review') return '←  ☒ Repo  ✔ Submit  →\n\nReview your answers\n\n● 下一個要 review 哪個 repo？\n  → robinstech-carbis-web\n\n❯ 1. Submit answers\n  2. Cancel\n\nEnter to select'
    return FULL.replace('❯ 1.', '  1.').replace('[ ] robinstech', `[${checked ? '✔' : ' '}] robinstech`)
      .replace(cursor === 5 ? '     Submit' : `  ${cursor + 1}.`, cursor === 5 ? '❯    Submit' : `❯ ${cursor + 1}.`)
  }
  await act(() => useStore.setState({
    readTerminal: async () => ({ text: screen() }) as TerminalSnapshot,
    sendKeys: async (_botId: string, keys: string[]) => {
      for (const key of keys) {
        sent.push(key)
        if (key === 'space') checked = !checked
        else if (key === 'down') cursor++
        else if (key === 'up') cursor--
        else if (key === 'enter') stage = stage === 'review' ? 'done' : 'review'
      }
    },
  } as never))
  try {
    const root = await mount(<BlockedDraft botId="repo-1887" menu={parseChoiceMenu(SHORT)!} />)
    await until(() => !!root.querySelector('.bc-submit'), '草稿載入完成')
    await act(() => root.querySelector<HTMLButtonElement>('.bc-item')!.click())
    await act(() => root.querySelector<HTMLButtonElement>('.bc-submit')!.click())
    await until(() => stage === 'done' || !!root.querySelector('.bc-stale'), '交卷或拒送結果', 6000)
    assert.equal(Boolean(root.querySelector('.bc-stale')), false, root.textContent ?? '')
    assert.equal(stage, 'done')
    assert.equal(checked, true)
    assert.deepEqual(sent, ['space', 'down', 'down', 'down', 'down', 'down', 'enter', 'enter'])
  } finally {
    Object.defineProperty(window, 'innerWidth', { configurable: true, value: width })
  }
})
