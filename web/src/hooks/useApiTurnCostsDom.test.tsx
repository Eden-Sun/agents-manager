import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { rawTransport } from '../api'
import {
  clearApiBotEntriesForTest,
  getApiBotCache,
  refreshApiBotUsage,
  useApiBotCache,
  useApiBotCacheSub,
} from './useApiTurnCosts'

before(setupDom)
afterEach(async () => {
  clearApiBotEntriesForTest()
  await unmountAll()
})
after(teardownDom)

test('useApiBotCacheSub：多顆 bot 共用一份輪詢，同 bot 複數訂閱只打一次 API', async () => {
  const calls: string[] = []
  const orig = rawTransport.request.bind(rawTransport)
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    if (path.includes('/api-usage')) {
      calls.push(path)
      return {
        attempts: { total: 1, recorded: 1, reserved: 0, unknown: 0, failed: 0 },
        cost_microusd: { recorded: 100, reserved: 0, unknown: 0 },
        tokens: { input: 10, output: 10, cache_read: 0, cache_creation: 0 },
        cache_hit_rate: null,
        turns: [],
        context: { tokens: 10, compact_at: null },
        cache: { last_request_at: '2026-11-15T04:00:00Z', ttl_secs: 300, expires_at: '2026-11-15T04:05:00Z', working: false },
        last: null,
      }
    }
    return orig(method as any, path, body)
  }) as typeof rawTransport.request

  function Consumer({ botIds }: { botIds: string[] }) {
    useApiBotCacheSub(botIds)
    return <div data-testid="consumer" />
  }

  function SingleConsumer({ botId }: { botId: string }) {
    const cache = useApiBotCache(botId)
    return <div data-testid="single">{cache?.ttl_secs ?? 0}</div>
  }

  try {
    // 同時掛載兩個 consumer，都包含 bot-a
    await mount(
      <>
        <Consumer botIds={['bot-a', 'bot-b']} />
        <SingleConsumer botId="bot-a" />
      </>,
    )

    await act(async () => {
      await new Promise((r) => setTimeout(r, 50))
    })

    // bot-a 只打一次，bot-b 打一次，共兩次（沒有因為兩個元件訂閱 bot-a 而打兩次）
    assert.equal(calls.filter((c) => c.includes('bot-a')).length, 1)
    assert.equal(calls.filter((c) => c.includes('bot-b')).length, 1)

    // getApiBotCache 可同步取得
    const cachedA = getApiBotCache('bot-a')
    assert.equal(cachedA?.ttl_secs, 300)
    assert.equal(cachedA?.expires_at, '2026-11-15T04:05:00Z')

    // refreshApiBotUsage 會觸發重抓
    await act(async () => {
      refreshApiBotUsage('bot-a')
      await new Promise((r) => setTimeout(r, 50))
    })
    assert.equal(calls.filter((c) => c.includes('bot-a')).length, 2)
  } finally {
    rawTransport.request = orig
  }
})
