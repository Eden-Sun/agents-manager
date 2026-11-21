import * as api from '../api'
import { ApiError } from '../api/types'
import { projectHostName, useStore } from '../store/store'
import { t } from './i18n'
import { watchLoginShell } from './loginShellWatch'
import { cliLoginCommand } from './quotaLogin'

export async function openFreshShellAndType(host: string, command: string): Promise<{ opened: boolean; typed: boolean }> {
  const key = `shell:${host}`
  const s = useStore.getState()
  if (s.busy[key]) return { opened: false, typed: false }
  useStore.setState((st) => ({ busy: { ...st.busy, [key]: true } }))
  try {
    let shell
    try {
      shell = await api.openHostShell(host)
    } catch (e) {
      s.notify('error', openShellError(host, e))
      return { opened: false, typed: false }
    }
    watchLoginShell(host, shell.pane_id)
    useStore.getState().viewHostShell(shell)
    try {
      await api.sendHostShellText(host, shell.pane_id, command, true)
      return { opened: true, typed: true }
    } catch {
      s.notify('info', t('shell 已開好，請在裡面輸入 {command}', { command }))
      return { opened: true, typed: false }
    }
  } finally {
    useStore.setState((st) => {
      const busy = { ...st.busy }
      delete busy[key]
      return { busy }
    })
  }
}

function openShellError(host: string, e: unknown): string {
  if (e instanceof ApiError && e.body.reason === 'too_many_shells') {
    const max = typeof e.body.max === 'number' ? t('（上限 {n} 個）', { n: e.body.max }) : ''
    return t('{host} 開著的 shell 太多{max}，請先關掉不用的再登入', { host: host === 'local' || !host ? t('本機') : host, max })
  }
  return t('開 shell 失敗：{error}', { error: e instanceof Error ? e.message : String(e) })
}

export async function startCliLogin(botId: string): Promise<boolean> {
  const s = useStore.getState()
  const bot = s.bots.find((b) => b.id === botId)
  if (!bot) return false
  const host = projectHostName(s, bot.project_id)
  if (bot.identity) return s.loginIdentity(host, bot.identity)
  if (!(await s.confirmCodexLoginReplace(host, bot.kind, null))) return false
  const { opened } = await openFreshShellAndType(host, cliLoginCommand(bot.kind, {}))
  return opened
}
