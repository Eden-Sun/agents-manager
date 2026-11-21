import { rawTransport } from './index'

export interface OpenAiApiBalanceAnchor {
  usd: number
  at: string
}

export interface OpenAiApiBalance {
  enabled: boolean
  configured: boolean
  error?: string
  as_of: string
  anchor?: OpenAiApiBalanceAnchor
  spent_usd?: number
  remaining_usd?: number
  uncertain_usd?: number
  remaining_floor_usd?: number
}

export async function fetchOpenAiApiBalance(): Promise<OpenAiApiBalance> {
  return (await rawTransport.request('GET', '/openai-api/balance')) as OpenAiApiBalance
}

export async function putOpenAiApiBalanceAnchor(usd: number, at?: string): Promise<OpenAiApiBalance> {
  const body = { usd, at }
  return (await rawTransport.request('PUT', '/openai-api/balance-anchor', body)) as OpenAiApiBalance
}
