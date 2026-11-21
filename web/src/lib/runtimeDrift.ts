import { effortLabel } from '../api/types.ts'
import { canonicalIdentity, defaultIdentityName } from './defaultIdentity.ts'
import type { Bot, Run } from '../api/types.ts'
import { modelMatches } from './modelMatches.ts'

export interface RuntimeDriftField {
  field: 'model' | 'effort' | 'fast' | 'identity'
  label: string
  running: string
  configured: string
}

function comparesFast(kind: string): boolean {
  return kind === 'codex'
}

const NONE = '（CLI 預設）'

function normalizedIdentity(value: string | null | undefined): string | null {
  const trimmed = value?.trim() ?? ''
  return trimmed || null
}

function identityLabel(value: string | null | undefined, kind: string): string {
  return canonicalIdentity(kind, value) ?? defaultIdentityName(kind) ?? '（預設）'
}

export function runtimeIdentity(run: Run | null): string | null | undefined {
  if (!run || (run.state !== 'running' && run.state !== 'starting')) return undefined
  if (typeof run.runtime_identity !== 'string') return undefined
  return normalizedIdentity(run.runtime_identity)
}

export function quotaIdentity(bot: { identity: string | null } | null | undefined, run: Run | null): string | null {
  const running = runtimeIdentity(run)
  return running === undefined ? (bot?.identity ?? null) : running
}

export function runtimeSettingsKnown(run: Run | null): boolean {
  if (!run || (run.state !== 'running' && run.state !== 'starting')) return false
  return run.runtime_model !== null || run.runtime_effort !== null || run.runtime_fast !== null
}

export function runningFast(bot: Bot, run: Run | null): boolean {
  const reported = run?.status?.fast_mode
  if (reported != null) return Boolean(reported)
  return Boolean(runtimeSettingsKnown(run) ? (run!.runtime_fast ?? bot.fast) : bot.fast)
}

export function runtimeKnown(run: Run | null): boolean {
  return runtimeSettingsKnown(run) || runtimeIdentity(run) !== undefined
}

function sameModel(running: string | null | undefined, configured: string | null | undefined): boolean {
  if (!running && !configured) return true
  return modelMatches(running, configured)
}

export const CLI_DEFAULT_EFFORT = 'default'

function runningEffort(value: string | null): string | null {
  return value === CLI_DEFAULT_EFFORT ? null : value
}

export function runtimeDrift(bot: Bot | null, run: Run | null): RuntimeDriftField[] {
  if (!bot || !run || !runtimeKnown(run)) return []
  const out: RuntimeDriftField[] = []
  const defaultsStay = (configured: string | null | undefined) => (configured ?? null) === null && !bot.needs_restart
  if (run.runtime_model !== null && !defaultsStay(bot.model) && !sameModel(run.runtime_model, bot.model)) {
    out.push({
      field: 'model',
      label: '模型',
      running: run.runtime_model ?? NONE,
      configured: bot.model ?? NONE,
    })
  }
  const effort = runningEffort(run.runtime_effort)
  if (run.runtime_effort !== null && !defaultsStay(bot.effort) && effort !== (bot.effort ?? null)) {
    out.push({
      field: 'effort',
      label: '強度',
      running: effort ? effortLabel(effort) : NONE,
      configured: bot.effort ? effortLabel(bot.effort) : NONE,
    })
  }
  if (comparesFast(bot.kind) && run.runtime_fast !== null && run.runtime_fast !== bot.fast) {
    out.push({ field: 'fast', label: 'fast', running: run.runtime_fast ? '開' : '關', configured: bot.fast ? '開' : '關' })
  }
  if (typeof run.runtime_identity === 'string') {
    const running = canonicalIdentity(bot.kind, run.runtime_identity)
    const configured = canonicalIdentity(bot.kind, bot.identity)
    if (running !== configured) {
      out.push({
        field: 'identity',
        label: '帳號',
        running: identityLabel(running, bot.kind),
        configured: identityLabel(configured, bot.kind),
      })
    }
  }
  return out
}

export function driftLine(d: RuntimeDriftField): string {
  return `${d.label} 實際 ${d.running}、設定 ${d.configured}`
}

const SHORT_LABEL: Record<RuntimeDriftField['field'], string> = { model: '模', effort: '強', fast: 'fast', identity: '帳' }

export function driftFieldsLabel(drift: RuntimeDriftField[], short = false): string {
  return drift.map((d) => (short ? SHORT_LABEL[d.field] : d.label)).join('/')
}

export function driftTitle(drift: RuntimeDriftField[]): string {
  if (!drift.length) return ''
  return `${drift.map(driftLine).join('\n')}\n重啟這顆 bot 才會換成設定的值。`
}

export function isFastOnlyDrift(kind: string, drift: RuntimeDriftField[]): boolean {
  return kind === 'codex' && drift.length > 0 && drift.every((d) => d.field === 'fast')
}
