import * as api from '../api'
import { projectHostName, useStore } from '../store/store'
import { cliLoginCommand } from './quotaLogin'

/**
 * 在獨立的主機 shell 用 CLI 登入這顆 bot 的身份（claude `auth login`），**不碰 bot 自己的 pane**
 * （UI-DECISIONS「CLI 登入不佔 bot pane」）：bot 可能正卡在 `Not logged in`，送 `/login` 進去只會讓它更卡。
 * 有身份走 daemon 的 `POST …/identities/{identity}/login`——env 由 daemon 照該主機偵測到的設定組（alias 換了
 * `CLAUDE_CONFIG_DIR` 也跟著換），登完重驗、收 pane；沒綁身份＝這台主機的預設帳號，開 shell 打不帶 env 的那行。
 */
export async function startCliLogin(botId: string): Promise<boolean> {
  const s = useStore.getState()
  const bot = s.bots.find((b) => b.id === botId)
  if (!bot) return false
  const host = projectHostName(s, bot.project_id)
  if (bot.identity) return s.loginIdentity(host, bot.identity)
  if (!(await s.openHostShell(host))) return false
  const view = useStore.getState().shellView
  if (!view) return false
  const command = cliLoginCommand(bot.kind, {})
  try {
    await api.sendHostShellText(host, view.paneId, command, true)
  } catch {
    // shell 已經開在眼前：告訴使用者自己敲哪一行最省事（同 `QuotaLoginShell`）。
    s.notify('info', `shell 已開好，請在裡面輸入 ${command}`)
  }
  return true
}
