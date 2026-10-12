/**
 * 主對話送出超過 daemon 能證明送達的長度（`MAX_PROVABLE_CHARS`，20 萬字）：daemon 回 422
 * `{error:"delivery_unprovable", reason:"prompt_too_long_to_prove", run_id, sent:false}`（不帶 message）。
 * 網頁的通知不能把 API 路徑（`POST /api/bots/…/prompt failed (422)`）或代碼原樣印出來。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { reset, routeDaemon } from './storeEnv.harness.ts'
import type { Bot, Project } from '../api/types.ts'

const { useStore } = await import('./store.ts')
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

test('超過可證明送達的長度：通知講人話，不貼路徑或代碼', async () => {
  reset()
  useStore.setState({
    bots: [{ id: 'b1', name: 'b1', project_id: 'p1', kind: 'claude' } as Bot],
    projects: [{ id: 'p1', path: '/p1', label: 'p1', host: 'local' } as Project],
  })
  routeDaemon((req) =>
    req.path.endsWith('/prompt')
      ? json({ error: 'delivery_unprovable', reason: 'prompt_too_long_to_prove', run_id: 'r1', sent: false }, 422)
      : json({}),
  )
  const ok = await useStore.getState().sendPrompt('b1', 'x'.repeat(10), [], false)
  assert.equal(ok, false)
  const texts = useStore.getState().notices.map((n) => n.text)
  assert.ok(texts.some((t) => /超過 20 萬字/.test(t)), JSON.stringify(texts))
  assert.ok(!texts.some((t) => /\/api\/|prompt_too_long|failed \(/.test(t)), JSON.stringify(texts))
})
