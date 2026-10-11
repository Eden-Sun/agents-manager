/**
 * 長時間開著的分頁：刪掉的 bot／專案不能在 store 的各張表裡留一輩子（`prune.ts`）。直接操作 store，不需要 DOM。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { reset, requests, routeDaemon } from './storeEnv.harness.ts'
import { noteMessagePageForTest, reloadLoadedConversations, storeMemoKeysForTest, useStore } from './store.ts'
import { pruneDeadKeys } from './prune.ts'
import type { Bot, Message, Mission, Project } from '../api/types.ts'

const bot = (id: string, project = 'p1') => ({ id, name: id, project_id: project, kind: 'claude', identity: null, herdr_session: null }) as Bot
const project = (id: string) => ({ id, label: id, path: `/${id}`, host: 'local' }) as Project
const msg = (id: string, botId: string) => ({ id, bot_id: botId, role: 'assistant', content: 'x' }) as unknown as Message
const mission = (id: string, projectId: string) => ({ id, project_id: projectId }) as unknown as Mission

function seed() {
  reset()
  useStore.setState({
    bots: [bot('b-live')],
    projects: [project('p-live')],
    messages: { 'b-live': [msg('m1', 'b-live')], 'b-gone': [msg('m2', 'b-gone')] },
    loadedBots: { 'b-live': true, 'b-gone': true },
    moreMessages: { 'b-live': true, 'b-gone': true, 'p-live': true, 'p-gone': true },
    loadingMore: { 'b-gone': false, 'p-gone': false },
    messageCapFloors: { 'b-live': 900, 'b-gone': 900, 'p-gone': 900 },
    liveReply: { 'b-gone': { turnId: 't', text: 'x', activity: '', alert: '', revision: 1 } },
    turns: { 'b-live': { t1: { id: 't1', status: 'queued' } }, 'b-gone': { t2: { id: 't2', status: 'queued' } } },
    messageLoadFailed: { 'b-live': true, 'b-gone': true },
    shareRev: { 'b-live': 1, 'b-gone': 3 },
    composerDrafts: {},
    previews: {},
    groupMessages: { 'p-live': [], 'p-gone': [] },
    loadedProjects: { 'p-live': true, 'p-gone': true },
    missions: { 'p-live': [mission('mi-live', 'p-live')], 'p-gone': [mission('mi-gone', 'p-gone')] },
    missionsCapped: { 'p-live': { done: false, cancelled: false }, 'p-gone': { done: true, cancelled: false } },
    missionDetail: { 'mi-live': {} as never, 'mi-gone': {} as never, 'mi-old': {} as never },
    missionLoading: { 'mi-live': false, 'mi-gone': false },
    missionLoadErrors: { 'mi-gone': 'x' },
    sidePanes: { 'p-live': [], 'p-gone': [] },
    botOrder: { 'p-live': ['b-live'], 'p-gone': ['b-gone'] },
    drafts: { 'bot:b-live': 'a', 'bot:b-gone': 'b', 'group:p-live': 'c', 'group:p-gone': 'd', 'shell:local/w1:p1': 'ls' },
    draftCursors: { 'bot:b-live': { start: 0, end: 0 }, 'bot:b-gone': { start: 1, end: 1 } },
  } as never)
}

test('pruneDead：刪掉的 bot／專案在各張表的 key 都被帶走，還在的與不歸它管的不動', () => {
  seed()
  const before = useStore.getState()
  const keepLive = before.messages['b-live']
  useStore.getState().pruneDead()
  const s = useStore.getState()
  assert.deepEqual(Object.keys(s.messages), ['b-live'])
  assert.equal(s.messages['b-live'], keepLive, '還在的 bot 的陣列是同一個參考（不重建）')
  assert.deepEqual(Object.keys(s.loadedBots), ['b-live'])
  assert.deepEqual(Object.keys(s.moreMessages).sort(), ['b-live', 'p-live'], '這張表的 key 可能是 bot 也可能是專案')
  assert.deepEqual(Object.keys(s.loadingMore), [])
  assert.deepEqual(Object.keys(s.messageCapFloors), ['b-live'])
  assert.deepEqual(Object.keys(s.liveReply), [])
  // 回合表、載入失敗旗標、分享版本號：已刪 bot 的 key 都帶走（#1342：turns 的 queued 回合留著會被重連反覆重抓）。
  assert.deepEqual(Object.keys(s.turns), ['b-live'])
  assert.deepEqual(Object.keys(s.messageLoadFailed), ['b-live'])
  assert.deepEqual(Object.keys(s.shareRev), ['b-live'])
  assert.deepEqual(Object.keys(s.groupMessages), ['p-live'])
  assert.deepEqual(Object.keys(s.loadedProjects), ['p-live'])
  assert.deepEqual(Object.keys(s.missions), ['p-live'])
  assert.deepEqual(Object.keys(s.missionsCapped), ['p-live'])
  assert.deepEqual(Object.keys(s.sidePanes), ['p-live'])
  assert.deepEqual(Object.keys(s.botOrder), ['p-live'])
  // 任務細節只留清單上還有的：專案沒了的、結案超過上限掉出清單的（mi-old）都不再有人看。
  assert.deepEqual(Object.keys(s.missionDetail), ['mi-live'])
  assert.deepEqual(Object.keys(s.missionLoading), ['mi-live'])
  assert.deepEqual(Object.keys(s.missionLoadErrors), [])
  // 草稿：bot／group 的看活不活；shell 草稿不是這裡管的。
  assert.deepEqual(Object.keys(s.drafts).sort(), ['bot:b-live', 'group:p-live', 'shell:local/w1:p1'])
  assert.deepEqual(Object.keys(s.draftCursors), ['bot:b-live'])
})

test('pruneDead：什麼都沒掉就不 set（不多一次 render）', () => {
  seed()
  useStore.getState().pruneDead()
  let renders = 0
  const off = useStore.subscribe(() => {
    renders += 1
  })
  useStore.getState().pruneDead()
  off()
  assert.equal(renders, 0)
})

test('pruneDead：軟刪除的 bot 復原（回到 bots 名單）後，訊息會重新載，不是帶著舊資料', () => {
  seed()
  useStore.getState().pruneDead()
  useStore.setState({ bots: [bot('b-live'), bot('b-gone')] })
  assert.equal(useStore.getState().loadedBots['b-gone'], undefined, '沒有「已載入」旗標＝選到它時會重載')
  assert.equal(useStore.getState().messages['b-gone'], undefined)
})

test('pruneDead：模組層的帳（頁代次）也只留還在名單上的 bot／專案', () => {
  seed()
  noteMessagePageForTest('bot', 'b-live')
  noteMessagePageForTest('bot', 'b-gone')
  noteMessagePageForTest('group', 'p-live')
  noteMessagePageForTest('group', 'p-gone')
  useStore.getState().pruneDead()
  assert.deepEqual(storeMemoKeysForTest().messagePageGenerations.sort(), ['bot:b-live', 'group:p-live'])
})

test('pruneDeadKeys：三張回合相關的表都只剩活著的 bot 時回 null（同一個參考，不多 set）', () => {
  seed()
  useStore.getState().pruneDead()
  assert.equal(pruneDeadKeys(useStore.getState()), null)
})

test('已刪 bot 留著的 queued 回合被帶走後，重連不再重抓它', async () => {
  seed()
  // 只留這顆已刪 bot 的 queued 回合；沒有已載入的對話、沒有選取，重連補抓只能靠 turns 把它列進去。
  useStore.setState({ loadedBots: {}, selectedBotId: null, turns: { 'b-gone': { t9: { id: 't9', status: 'queued' } as never } } } as never)
  useStore.getState().pruneDead()
  assert.equal(useStore.getState().turns['b-gone'], undefined, '已刪 bot 的回合表要帶走')
  routeDaemon((r) => (r.path.includes('/messages') ? new Response(JSON.stringify({ messages: [], turns: [], has_more: false }), { status: 200 }) : new Response('{}', { status: 200 })))
  requests.length = 0
  await reloadLoadedConversations(useStore.getState)
  assert.equal(
    requests.some((r) => r.path.includes('/bots/b-gone/messages')),
    false,
    '重連不再整頁重抓已刪的 bot',
  )
})
