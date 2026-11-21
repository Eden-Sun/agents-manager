import assert from 'node:assert/strict'
import type { MockTransport } from './mock.ts'
import { manualMockClock } from './mockClock.ts'

export interface MockBotRow { id: string; kind: string; run?: { state?: string } | null }
export interface MockMsg { id: string; role: string; turn_id: string | null; content: string; keep_warm?: boolean; sent_via?: string | null }

export const mockBots = (mock: MockTransport) => async () =>
  ((await mock.request('GET', '/state')) as { projects: { bots: MockBotRow[] }[] }).projects.flatMap((p) => p.bots)

export async function startMockBot(mock: MockTransport, kind: string): Promise<string> {
  const bots = mockBots(mock)
  const b = (await bots()).find((x) => x.kind === kind)
  assert.ok(b, `seed 要有 ${kind}`)
  await mock.request('POST', `/bots/${b.id}/start`, {})
  for (let i = 0; i < 100; i++) {
    if ((await bots()).find((x) => x.id === b.id)?.run?.state === 'running') return b.id
    await manualMockClock.advance(250)
  }
  throw new Error(`mock 的 ${kind} 沒起來`)
}

export function mockClaudeBot(mock: MockTransport): () => Promise<string> {
  let started: Promise<string> | null = null
  return () => (started ??= startMockBot(mock, 'claude'))
}

export const mockMessages = (mock: MockTransport) => async (botId: string, qs: string) =>
  ((await mock.request('GET', `/bots/${botId}/messages${qs}`)) as { messages: MockMsg[] }).messages
