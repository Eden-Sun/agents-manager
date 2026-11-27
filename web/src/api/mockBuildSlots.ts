import type { BuildSlots } from './buildSlots'

export type MockBuildScene = 'idle' | 'busy' | 'queued'

const ago = (s: number) => new Date(Date.now() - s * 1000).toISOString()

export function mockBuildSlots(scene: MockBuildScene, botIds: string[]): BuildSlots {
  const [a = null, b = null] = botIds
  const remote = { enabled: true, hosts: [{ target: 'local', local: true, active: 0, waiting: 0 }, { target: 'ubuntu@build-box:22', local: false, active: 0, waiting: 0 }] }
  if (scene === 'idle') return { max_concurrent: 2, active: 0, slots: [], remote, remote_runs: [], total_active: 0, total_waiting: 0 }
  const slots = [{ holder: 'mock:1', status: 'held', bot_id: a, purpose: 'test -p agents-managerd', host: 'local', since: ago(95) }]
  if (scene === 'queued') {
    slots.push({ holder: 'mock:2', status: 'held', bot_id: b, purpose: 'build --release', host: 'local', since: ago(40) })
    slots.push({ holder: 'mock:3', status: 'waiting', bot_id: a, purpose: 'clippy --workspace', host: 'local', since: ago(12) })
  }
  const queued = scene === 'queued' ? 1 : 0
  return {
    max_concurrent: 2,
    active: slots.filter((s) => s.status === 'held').length,
    slots,
    remote: { enabled: true, hosts: [{ target: 'local', local: true, active: 1, waiting: queued }, { target: 'ubuntu@build-box:22', local: false, active: null, waiting: null }] },
    remote_runs: [
      { call_id: 'MOCK1', bot_id: b, purpose: 'test --workspace', since: ago(210), target: 'local' },
      ...(queued ? [{ call_id: 'MOCK2', bot_id: a, purpose: 'check', since: ago(8), target: null }] : []),
    ],
    total_active: slots.filter((s) => s.status === 'held').length + 1,
    total_waiting: slots.filter((s) => s.status === 'waiting').length + queued,
  }
}
