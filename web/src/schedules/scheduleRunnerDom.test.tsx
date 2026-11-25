import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import type { ScheduleDetail, ScheduleOccurrence } from '../api/schedules'
import { readFileSync } from 'node:fs'
import { act } from 'react'
import { resetLangForTest, setLangPref } from '../lib/i18n'
import { useStore } from '../store/store'
import { ScheduleEditor } from './ScheduleEditor'

before(setupDom)
after(async () => {
  for (const notice of useStore.getState().notices) useStore.getState().dismiss(notice.id)
  await teardownDom()
})
afterEach(unmountAll)
beforeEach(() => {
  useStore.setState({
    projects: [{ id: 'p1', label: '專案', path: '/project', host: 'local' }],
    bots: [{ id: 'b1', name: 'WT-ON-JUll', project_id: 'p1', kind: 'agy', role: 'scheduler', model: null, effort: null, managed_by: 'user', parent_bot_id: null, share_profile: null }],
    runs: {}, notices: [], schedulesRev: 0,
  } as never)
})

function occ(id: string, runner: ScheduleOccurrence['runner']): ScheduleOccurrence {
  return {
    id, schedule_id: 's1', source: 'scheduled', scheduled_for: '2030-01-01T00:00:00Z', generation: 1, bot_id: 'b1', prompt: 'x',
    status: 'done', waiting_reason: null, merged_count: 0, conversation_id: null, turn_id: null, error: null,
    outcome: 'success', outcome_summary: 'ok', attempts: [], created_at: '', updated_at: '', runner,
  }
}

test('最近執行每筆顯示當時的 bot 與模型，舊紀錄不顯示', async () => {
  const detail: ScheduleDetail = {
    id: 's1', project_id: 'p1', bot_id: 'b1', name: '排程', prompt: 'x', mode: 'daily', spec: { mode: 'daily', time: '09:00' }, tz: 'UTC',
    notify: 'failure', clear_after: false, enabled: true, generation: 1, next_due: null, created_at: '', updated_at: '', open: null,
    recent: [
      occ('o3', { bot_name: 'WT-ON-JUll', kind: 'agy', model: 'gemini-4-hydrogen-low', effort: null, identity: null, source: 'run' }),
      occ('o2', { bot_name: 'wt-click', kind: 'codex', model: 'gpt-5.5', effort: 'high', identity: 'co0', source: 'bot' }),
      occ('o1', null),
    ],
  }
  fakeApi(() => detail)
  await mount(<ScheduleEditor projectId="p1" scheduleId="s1" onClose={() => undefined} onSelect={() => undefined} />)
  await settle()
  const tags = [...document.querySelectorAll<HTMLElement>('.schedule-runner')]
  assert.equal(tags.length, 2)
  assert.equal(tags[0].textContent, 'WT-ON-JUllagy 4-hydrogenLow')
  assert.equal(tags[0].dataset.runnerKind, 'agy')
  assert.match(tags[1].textContent ?? '', /^wt-clickcodex .*High$/)
  assert.match(tags[1].title, /co0/)
  assert.notEqual(tags[0].textContent, tags[1].textContent)
})

test('英文介面下最近執行的狀態字樣走翻譯', async () => {
  const detail = {
    id: 's1', project_id: 'p1', bot_id: 'b1', name: '排程', prompt: 'x', mode: 'daily', spec: { mode: 'daily', time: '09:00' }, tz: 'UTC',
    notify: 'failure', clear_after: false, enabled: true, generation: 1, next_due: null, created_at: '', updated_at: '', open: null,
    recent: [occ('o1', null)],
  } as ScheduleDetail
  fakeApi(() => detail)
  await act(async () => setLangPref('en'))
  try {
    await mount(<ScheduleEditor projectId="p1" scheduleId="s1" onClose={() => undefined} onSelect={() => undefined} />)
    await settle()
    const meta = document.querySelector('.schedule-history-meta')?.textContent ?? ''
    assert.match(meta, /Done/)
    assert.doesNotMatch(meta, /已完成/)
  } finally {
    await act(async () => resetLangForTest())
  }
})

test('排程編輯器的 grid 單欄用 minmax(0,1fr)，窄螢幕不被最寬的子項撐出視窗', () => {
  const css = readFileSync(new URL('./scheduleEditor.css', import.meta.url), 'utf8')
  assert.match(css, /\.schedule-editor\s*\{[^}]*grid-template-columns:\s*minmax\(0,\s*1fr\)/)
  const runner = readFileSync(new URL('./runnerTag.css', import.meta.url), 'utf8')
  assert.match(runner, /\.schedule-runner-bot\s*\{[^}]*text-overflow:\s*ellipsis/)
})
