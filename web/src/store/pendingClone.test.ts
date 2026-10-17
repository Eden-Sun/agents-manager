/**
 * 分身的灰色佔位列只靠建立請求的冪等鍵（create_request_id）對到真 bot，不靠名字（#1796）：
 * 建立期間真 bot 被改名佔位不會殘留，同名的既有 bot 也吃不掉佔位。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { reset, routeDaemon } from './storeEnv.harness.ts'
import type { Bot, Project } from '../api/types.ts'

const { useStore } = await import('./store.ts')

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })
const project = () => ({ id: 'p1', label: 'p', path: '/p', host: 'local' }) as Project
const bot = (id: string, name: string, extra: Partial<Bot> = {}) =>
  ({ id, name, project_id: 'p1', kind: 'claude', identity: null, ...extra }) as Bot
const placeholder = () => bot('pending:x', 'b1-2', { pending: true, create_request_id: 'crid-x' })

function snapshot(bots: Bot[]) {
  reset()
  routeDaemon((r) => {
    if (r.path.endsWith('/state')) return json({ daemon_seq: 1, projects: [{ ...project(), bots }], bots, runs: [], turns: [] })
    return json({})
  })
  useStore.setState({ projects: [project()], bots: [bot('b1', 'b1'), placeholder()], botOrder: {}, projectOrder: [] } as never)
}

test('建立期間真 bot 改了名：照冪等鍵換掉佔位，不留殘影', async () => {
  snapshot([bot('b1', 'b1'), bot('c1', '衣服-PM', { create_request_id: 'crid-x' })])
  await useStore.getState().refreshState()
  assert.deepEqual(useStore.getState().bots.map((b) => b.id), ['b1', 'c1'])
})

test('同名的既有 bot 不會吃掉佔位：鍵對不上就留著等真的那顆', async () => {
  snapshot([bot('b1', 'b1'), bot('other', 'b1-2', { create_request_id: 'crid-other' }), bot('plain', 'b1-2')])
  await useStore.getState().refreshState()
  const ids = useStore.getState().bots.map((b) => b.id)
  assert.ok(ids.includes('pending:x'), '佔位還在')
  assert.ok(ids.includes('other') && ids.includes('plain'), '既有 bot 原樣')
})
