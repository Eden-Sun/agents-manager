import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { rawTransport } from '../api'
import { ProjectLineageSection } from './ProjectLineageSection'

before(setupDom)
afterEach(unmountAll)
after(teardownDom)

test('ProjectLineageSection: 渲染血統統計列表（檢查點、SHA、認領中、命中率、省下 token）', async () => {
  const origRequest = rawTransport.request.bind(rawTransport)
  const mockStats = {
    project_id: 'p-1',
    lines: [
      {
        line: 'web',
        checkpoint: {
          id: 'cp-1',
          project_id: 'p-1',
          line: 'web',
          role: 'worker',
          kind: 'codex',
          session_id: '019a16b82ea4f7062ada9d3c819e0e738',
          parent_session_id: null,
          context_tokens: 145000,
          input_tokens: 200000,
          cached_input_tokens: 160000,
          output_tokens: 5000,
          last_api_at: '2026-11-21T10:00:00Z',
          head_sha: 'c3e49701c4abcdef',
          state: 'checkpoint',
          summary: 'web checkpoint',
          created_at: '2026-11-21T08:00:00Z',
          updated_at: '2026-11-21T10:00:00Z',
        },
        open_claims_count: 2,
        recent_claims: [],
        tokens: {
          input_tokens: 200000,
          cached_input_tokens: 160000,
          output_tokens: 5000,
          hit_ratio: 0.8,
          saved_input_tokens: 160000,
        },
      },
      {
        line: 'lifecycle',
        checkpoint: null,
        open_claims_count: 0,
        recent_claims: [],
        tokens: {
          input_tokens: 0,
          cached_input_tokens: 0,
          output_tokens: 0,
          hit_ratio: 0.0,
          saved_input_tokens: 0,
        },
      },
    ],
  }

  rawTransport.request = (async (_method: string, path: string) => {
    if (path.includes('/lineage/stats')) {
      return mockStats
    }
    return {}
  }) as typeof rawTransport.request

  try {
    await mount(<ProjectLineageSection projectId="p-1" />)
    await act(async () => {
      await new Promise((r) => setTimeout(r, 20))
    })

    const bodyText = document.body.textContent!
    assert.match(bodyText, /Session 血統與快取節省/)
    assert.match(bodyText, /web/)
    assert.match(bodyText, /lifecycle/)

    // web 線的檢查點 short id (019a16b8) 與 short sha (c3e4970)
    assert.match(bodyText, /019a16b8/)
    assert.match(bodyText, /c3e4970/)

    // 認領中
    assert.match(bodyText, /2 筆/)

    // 命中率與省下的 token
    assert.match(bodyText, /80\.0%/)
    assert.match(bodyText, /160k/)

    // lifecycle 線沒有檢查點與 token
    const rows = document.querySelectorAll('.project-lineage-row')
    assert.equal(rows.length, 2)
  } finally {
    rawTransport.request = origRequest
  }
})

test('ProjectLineageSection: 空狀態提示', async () => {
  const origRequest = rawTransport.request.bind(rawTransport)
  rawTransport.request = (async (_method: string, path: string) => {
    if (path.includes('/lineage/stats')) {
      return { project_id: 'p-empty', lines: [] }
    }
    return {}
  }) as typeof rawTransport.request

  try {
    await mount(<ProjectLineageSection projectId="p-empty" />)
    await act(async () => {
      await new Promise((r) => setTimeout(r, 20))
    })

    assert.match(document.body.textContent!, /目前尚無血統紀錄/)
  } finally {
    rawTransport.request = origRequest
  }
})
