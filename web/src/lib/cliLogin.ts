import * as api from '../api'
import { ApiError } from '../api/types'
import { projectHostName, useStore } from '../store/store'
import { cliLoginCommand } from './quotaLogin'

/**
 * 在**新開**的主機 shell 裡打一行登入指令，畫面切過去（claude `auth login`、codex／grok／agy 的登入）。
 *
 * 不能走 `store.openHostShell(host)`：它會接回這台主機最近一個還活著的 shell，指令就被打進那顆 shell 的前景程式
 * （vim、sudo 密碼提示、手動開的 claude…，#915）。這裡直接叫 daemon 新開（`POST /hosts/{name}/shells` 每次都是新 tab），
 * 確認框「開一個 shell」的承諾才成立。
 *
 * `opened`：shell 開起來了（失敗已經用 `notify('error')` 講過原因）；`typed`：指令也打進去了
 * （打字失敗時 shell 仍在使用者眼前，已提示自己敲哪一行）。
 */
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
    // 不動 selectedBotId：shell 掛在目前 bot 標題列底下（同 `store.openHostShell`）。
    useStore.getState().viewHostShell(shell)
    try {
      await api.sendHostShellText(host, shell.pane_id, command, true)
      return { opened: true, typed: true }
    } catch {
      // 打字失敗不代表 shell 沒開起來：使用者眼前就是那個終端，告訴他自己敲哪一行最省事。
      s.notify('info', `shell 已開好，請在裡面輸入 ${command}`)
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
    const max = typeof e.body.max === 'number' ? `（上限 ${e.body.max} 個）` : ''
    return `${host === 'local' || !host ? '本機' : host} 開著的 shell 太多${max}，請先關掉不用的再登入`
  }
  return `開 shell 失敗：${e instanceof Error ? e.message : String(e)}`
}

/**
 * 在獨立的主機 shell 用 CLI 登入這顆 bot 的身份（claude `auth login`），**不碰 bot 自己的 pane**
 * （UI-DECISIONS「CLI 登入不佔 bot pane」）：bot 可能正卡在 `Not logged in`，送 `/login` 進去只會讓它更卡。
 * 有身份走 daemon 的 `POST …/identities/{identity}/login`——env 由 daemon 照該主機偵測到的設定組（alias 換了
 * `CLAUDE_CONFIG_DIR` 也跟著換），登完重驗、收 pane；沒綁身份＝這台主機的預設帳號，開新 shell 打不帶 env 的那行。
 */
export async function startCliLogin(botId: string): Promise<boolean> {
  const s = useStore.getState()
  const bot = s.bots.find((b) => b.id === botId)
  if (!bot) return false
  const host = projectHostName(s, bot.project_id)
  if (bot.identity) return s.loginIdentity(host, bot.identity)
  const { opened } = await openFreshShellAndType(host, cliLoginCommand(bot.kind, {}))
  return opened
}
