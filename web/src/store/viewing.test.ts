/**
 * issue #509：shell 面板蓋住畫面時，看不到的那段對話不可以被標成已讀。
 * bot 側本來就守住了，群組側沒有——這裡兩邊都釘住，免得下次只修一邊。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { reset } from './storeEnv.harness.ts'
import type { Bot, Project } from '../api/types.ts'

const { useStore } = await import('./store.ts')

const g = globalThis as unknown as Record<string, unknown>
const hidden = g.document

/** `markCurrentRead` 只在視窗回到前景時才做事（`App.tsx` 的 focus handler）。 */
function focused<T>(body: () => T): T {
  g.document = { visibilityState: 'visible', hasFocus: () => true, addEventListener: () => {}, removeEventListener: () => {} }
  try {
    return body()
  } finally {
    g.document = hidden
  }
}

const bot = (id: string) => ({ id, name: id, project_id: 'p1', kind: 'claude', identity: null }) as Bot
const project = () => ({ id: 'p1', label: 'p', path: '/p', host: 'local' }) as Project
const shell = { host: 'local', paneId: 'w1:p9', cwd: '/p' }

function seed(extra: Record<string, unknown>) {
  reset()
  useStore.setState({
    projects: [project()],
    bots: [bot('b1')],

    messages: {},
    botUnread: {},

    selectedBotId: null,
    selectedProjectId: null,
    shellView: null,
    ...extra,
  })
}


test('選著 bot 又開了 shell：bot 未讀一樣守住（原本就對，別修回去）', () => {
  seed({ selectedBotId: 'b1', shellView: shell, botUnread: { b1: 3 } })
  focused(() => useStore.getState().markCurrentRead())
  assert.deepEqual(useStore.getState().botUnread, { b1: 3 })
})
