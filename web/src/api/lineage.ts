/**
 * Codex session 血統與快取統計的 REST API 封裝（issue #1943）。
 */
import { rawTransport } from './index'

export interface LineageCheckpoint {
  id: string
  project_id: string
  line: string
  role: string
  kind: string
  session_id: string
  parent_session_id: string | null
  context_tokens: number | null
  input_tokens: number
  cached_input_tokens: number
  output_tokens: number
  last_api_at: string | null
  head_sha: string | null
  state: string
  summary: string | null
  created_at: string
  updated_at: string
}

export interface LineageClaimItem {
  id: string
  project_id: string
  line: string
  mode: 'resume' | 'fork'
  holder_bot_id: string | null
  base_session_id: string
  base_head_sha: string | null
  backup_session_id: string | null
  work_session_id: string | null
  state: 'open' | 'advanced' | 'abandoned'
  created_at: string
  updated_at: string
}

export interface LineageTokens {
  input_tokens: number
  cached_input_tokens: number
  output_tokens: number
  hit_ratio: number
  saved_input_tokens: number
}

export interface LineageLineStat {
  line: string
  checkpoint: LineageCheckpoint | null
  open_claims_count: number
  recent_claims: LineageClaimItem[]
  tokens: LineageTokens
}

export interface LineageStatsResponse {
  project_id: string
  lines: LineageLineStat[]
}

/** 取得專案的血統統計與快取節省數據。 */
export async function fetchLineageStats(projectId: string): Promise<LineageStatsResponse> {
  return (await rawTransport.request('GET', `/projects/${encodeURIComponent(projectId)}/lineage/stats`)) as LineageStatsResponse
}
