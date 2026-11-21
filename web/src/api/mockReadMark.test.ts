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
