const initialSearch = typeof location === 'undefined' ? '' : location.search
const day = (d: number) => new Date(Date.now() + d * 86_400_000).toISOString().slice(0, 10)

export function mockOpenAiBalance(search: string = initialSearch): unknown {
  const mode = new URLSearchParams(search).get('mock_openai_console') ?? 'ok'
  const now = new Date().toISOString()
  const base = { enabled: true, configured: true, anchor: { usd: 20, at: '2026-11-10T00:00:00.000Z' }, spent_usd: 2, remaining_usd: 18, uncertain_usd: 0, remaining_floor_usd: 18, as_of: now }
  if (mode === 'logged_out') return { ...base, console: { status: 'logged_out', fetched_at: now, error: 'login_rejected' } }
  return {
    ...base,
    console: {
      status: 'ok',
      total_available_usd: 82.5,
      total_granted_usd: 120,
      total_used_usd: 37.5,
      grants: [
        { remaining_usd: 70, total_usd: 100, expires_on: day(mode === 'ok' ? 3 : 40) },
        { remaining_usd: 12.5, total_usd: 20, expires_on: day(90) },
      ],
      org_title: 'Mock Org',
      fetched_at: now,
    },
  }
}
