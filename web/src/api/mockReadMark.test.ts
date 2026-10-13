/** mock 的已讀標記要跟 API.md §6 同形：省略 `at`、帶 `message_id` 時，標記時間取那則訊息的 `created_at`，不是現在。 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { MockTransport } from './mock.ts'

const mock = new MockTransport()

test('POST /bots/{id}/read 只帶 message_id：read_mark.at 是那則訊息的時間', async () => {
  const st = (await mock.request('GET', '/state')) as { projects: { bots: { id: string }[] }[] }
  const botId = st.projects[0].bots[0].id
  const { messages } = (await mock.request('GET', `/bots/${botId}/messages`)) as { messages: { id: string; created_at: string }[] }
  const m = messages[messages.length - 1]
  assert.ok(m, 'seed 的第一顆 bot 要有訊息')
  const out = (await mock.request('POST', `/bots/${botId}/read`, { message_id: m.id })) as { read_mark: { at: string; id: string } }
  assert.equal(out.read_mark.id, m.id)
  assert.equal(out.read_mark.at, m.created_at)
})

test('POST /projects/{id}/group/read 只帶 message_id：read_mark.at 也是那則訊息的時間', async () => {
  const st = (await mock.request('GET', '/state')) as { projects: { id: string; bots: { id: string }[] }[] }
  const project = st.projects.find((p) => p.bots.length > 0)
  assert.ok(project, 'seed 要有一個專案有 bot')
  const { messages } = (await mock.request('GET', `/projects/${project.id}/messages`)) as { messages: { id: string; created_at: string }[] }
  const m = messages[messages.length - 1]
  assert.ok(m, 'seed 的專案要有訊息')
  const out = (await mock.request('POST', `/projects/${project.id}/group/read`, { message_id: m.id })) as { read_mark: { at: string; id: string } }
  assert.equal(out.read_mark.id, m.id)
  assert.equal(out.read_mark.at, m.created_at)
})
