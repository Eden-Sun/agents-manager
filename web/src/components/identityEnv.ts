import { t } from '../lib/i18n'
import type { BotKind } from '../api/types'

export function parseEnvText(text: string): Record<string, string> {
  const out: Record<string, string> = {}
  for (const raw of text.split('\n')) {
    const line = raw.trim()
    if (!line || line.startsWith('#')) continue
    const i = line.indexOf('=')
    if (i <= 0) continue
    out[line.slice(0, i).trim()] = line.slice(i + 1).trim()
  }
  return out
}

const RUST_WS = '\\t\\n\\v\\f\\r \\u0085\\u00a0\\u1680\\u2000-\\u200a\\u2028\\u2029\\u202f\\u205f\\u3000'
const RUST_TRIM = new RegExp(`^[${RUST_WS}]+|[${RUST_WS}]+$`, 'g')
export const rustTrim = (s: string): string => s.replace(RUST_TRIM, '')

const ENV_NAME = /^[A-Za-z_][A-Za-z0-9_]*$/

export function checkEnvText(text: string): { env: Record<string, string>; errors: string[] } {
  const env: Record<string, string> = {}
  const errors: string[] = []
  text.split('\n').forEach((raw, idx) => {
    const line = rustTrim(raw)
    if (!line || line.startsWith('#')) return
    const n = idx + 1
    const i = line.indexOf('=')
    if (i < 0) {
      errors.push(t('第 {n} 行「{line}」沒有 =（要寫成 KEY=VALUE）', { n, line }))
      return
    }
    const key = rustTrim(line.slice(0, i))
    if (!key) {
      errors.push(t('第 {n} 行的 key 是空的（要寫成 KEY=VALUE）', { n }))
      return
    }
    if (!ENV_NAME.test(key)) {
      errors.push(t('第 {n} 行 key「{key}」不是合法的環境變數名稱（只能英數與 _，不能數字開頭）', { n, key }))
      return
    }
    const value = rustTrim(line.slice(i + 1))
    if (!value) {
      errors.push(t('第 {n} 行 {key} 的值是空的（空值等於沒設，身份會悄悄用預設帳號；要就寫上值，不要就刪掉這一行）', { n, key }))
      return
    }
    if (Object.hasOwn(env, key)) errors.push(t('第 {n} 行 key「{key}」重複了（前面已經有一行）', { n, key }))
    Object.defineProperty(env, key, { value, enumerable: true, writable: true, configurable: true })
  })
  return { env, errors }
}

const SECRET_KEY = /(KEY|TOKEN|SECRET|PASSW|CREDENTIAL)/i

export function envDisplayText(env: Record<string, string>): string {
  return Object.entries(env)
    .map(([k, v]) => `${k}=${SECRET_KEY.test(k) && v ? '••••••' : v}`)
    .join('\n')
}

export function emptyEnvKeys(env: Record<string, string>): string[] {
  return Object.entries(env)
    .filter(([, v]) => !rustTrim(v))
    .map(([k]) => k)
}

export const validIdentityName = (name: string): boolean => /^[a-z][a-z0-9_-]{0,31}$/.test(name)

export function identityHomeVar(kind: BotKind): string | undefined {
  return HOME_VAR[kind]
}
const HOME_VAR: Partial<Record<BotKind, string>> = {
  claude: 'CLAUDE_CONFIG_DIR',
  codex: 'CODEX_HOME',
  grok: 'GROK_HOME',
  agy: 'HOME',
}

const DEFAULT_HOME_DIR: Partial<Record<BotKind, string>> = {
  claude: '$HOME/.claude',
  codex: '$HOME/.codex',
  grok: '$HOME/.grok',
}

export function suggestedIdentityDir(kind: BotKind, name: string): string {
  return `$HOME/.${kind}-${name}`
}

export function suggestedIdentityEnvText(kind: BotKind, name: string): string {
  const v = identityHomeVar(kind)
  return v ? `${v}=${suggestedIdentityDir(kind, name)}` : ''
}

export function normalizeHomeDir(dir: string): string {
  return rustTrim(dir)
    .replace(/^(~|\$HOME|\$\{HOME\})(?=\/|$)/, '$HOME')
    .replace(/\/+$/, '')
}

export function identityHomeDir(kind: BotKind, env: Record<string, string>): string | null {
  const v = identityHomeVar(kind)
  if (!v || !Object.hasOwn(env, v)) return null
  const dir = rustTrim(env[v])
  return dir ? dir : null
}

export function sharedHomeWith(
  kind: BotKind,
  env: Record<string, string>,
  host: string,
  existing: readonly { name: string; kind: BotKind; host?: string | null; env: Record<string, string> }[],
): { names: string[]; defaultAccount: boolean } {
  const dir = identityHomeDir(kind, env)
  if (!dir) return { names: [], defaultAccount: false }
  const norm = normalizeHomeDir(dir)
  const names = existing
    .filter((i) => i.kind === kind && (i.host || 'local') === host)
    .filter((i) => {
      const other = identityHomeDir(kind, i.env)
      return other !== null && normalizeHomeDir(other) === norm
    })
    .map((i) => i.name)
  const def = DEFAULT_HOME_DIR[kind]
  const defaultAccount = def !== undefined && normalizeHomeDir(def) === norm
  return { names, defaultAccount }
}

export const IDENTITY_KINDS: readonly BotKind[] = ['claude', 'codex', 'grok', 'agy']
