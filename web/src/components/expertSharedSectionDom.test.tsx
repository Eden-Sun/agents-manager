import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import type { Bot, Project } from '../api/types'
import { Sidebar } from './Sidebar'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const project = { id: 'p1', label: 'proj', path: '/p1', host: 'local', mode: 'expert', workspace_id: null, github: null, created_at: '' } as unknown as Project

const bot = (id: string, over: Partial<Bot> = {}): Bot =>
  ({
    id, name: `bot-${id}`, project_id: 'p1', kind: 'claude', identity: null, model: 'haiku', effort: null,
    args: [], env: {}, autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user',
    parent_bot_id: null, primary: false, primary_position: 0, cwd: null,
    role: 'executor', manager_use: true, track: null, runtime_override: null, cluster: null, share_profile: null, ...over,
  }) as unknown as Bot

function seed() {
  useStore.setState({
    projects: [project],
    bots: [
      bot('m', { role: 'manager' }),
      bot('exec', { role: 'executor' }),
      bot('unset', { role: null }),
      bot('share-unset', { role: null, share_profile: 'restricted' }),
      bot('share-spec', { role: 'specialist', share_profile: 'trusted' }),
    ],
    runs: {},
    selectedProjectId: 'p1',
    selectedBotId: 'm',
    busy: {},
    notices: [],
  } as never)
}

const sectionTitles = () => [...document.querySelectorAll<HTMLElement>('.expert-section-title')].map((e) => e.textContent)
const rowIds = () => [...document.querySelectorAll<HTMLElement>('.bot-row[data-bot-id]')].map((e) => e.dataset.botId)
const unsetBadgeOn = (id: string) => document.querySelector<HTMLElement>(`.bot-row[data-bot-id="${id}"] .bot-unset-role`)

test('專家模式側欄：分享 bot 歸「分享用」分區、排在執行者之後，且不顯示未設角色徽章', async () => {
  seed()
  await mount(<Sidebar />)
  await settle()
  assert.deepEqual(sectionTitles(), ['管理者', '執行者', '分享用'])
  const ids = rowIds()
  assert.ok(ids.indexOf('share-unset') > ids.indexOf('exec'))
  assert.ok(ids.indexOf('share-spec') > ids.indexOf('exec'))
  assert.equal(unsetBadgeOn('share-unset'), null)
  assert.ok(unsetBadgeOn('unset'))
})
