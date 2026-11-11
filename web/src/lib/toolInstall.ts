/**
 * 主機缺工具／黃字提醒的引導（一致性區塊、gh 狀態列共用）。
 *
 * 只給指令與官方連結，不自動執行：沒有經過核對的一鍵安裝機制（claude／codex 的「安裝」鈕走的是既有 bot 流程，
 * rtk／bun／gh 沒有）。OS 用 daemon 量到的 `baseline.os`（`Darwin`／`Linux`）；不知道就兩套都列。
 */

export interface ToolInstallStep {
  /** 步驟前的小字，例 `macOS` `Linux`。 */
  label: string
  /** 可複製的指令。 */
  command: string
}

export interface ToolInstallInfo {
  steps: ToolInstallStep[]
  /** 官方頁；沒有經過核對的來源就不給，寧可不給也不猜網址。 */
  link: string | null
}

/** 每個工具的安裝指令。key 是 daemon 清單裡的工具名（`tool.<name>` 的 `<name>`）。 */
const INSTALL: Record<string, { mac?: string; linux?: string; link: string | null }> = {
  bun: { mac: 'brew install oven-sh/bun/bun', linux: 'curl -fsSL https://bun.sh/install | bash', link: 'https://bun.sh' },
  gh: { mac: 'brew install gh', linux: 'sudo apt install gh', link: 'https://cli.github.com' },
  jq: { mac: 'brew install jq', linux: 'sudo apt install jq', link: 'https://jqlang.github.io/jq/download/' },
  zsh: { mac: 'brew install zsh', linux: 'sudo apt install zsh', link: 'https://www.zsh.org/' },
  herdr: { mac: 'brew install herdr', linux: 'curl -fsSL https://herdr.dev/install.sh | sh', link: 'https://herdr.dev' },
  // rtk：目前沒有經過核對的官方安裝來源，不猜指令、不給連結。
  rtk: { link: null },
}

/** 工具的安裝引導；清單裡沒有的工具回 null（只顯示通用的 PATH 說明）。 */
export function toolInstallInfo(tool: string, os: string | null): ToolInstallInfo | null {
  const entry = INSTALL[tool]
  if (!entry) return null
  const steps: ToolInstallStep[] = []
  const showMac = os === null || os === 'Darwin'
  const showLinux = os === null || os === 'Linux'
  if (showMac && entry.mac) steps.push({ label: 'macOS', command: entry.mac })
  if (showLinux && entry.linux) steps.push({ label: 'Linux', command: entry.linux })
  return { steps, link: entry.link }
}

/**
 * 黃字提醒（warn）是什麼意思、要不要處理。沒列到的 id 回 null，照舊只顯示 daemon 的文字。
 * 前綴比對：`codex.config.toml:<鍵>` 是 config.toml 裡缺的鍵。
 */
export function baselineWarnWhy(id: string): string | null {
  if (id === 'codex.config.toml') return 'Codex 的 config.toml 不在，會用內建預設跑；agents-manager 需要的設定不會生效。有「可自動補」就用一致性補齊；「需手動」照提示自己加。'
  if (id === 'codex.hooks.json') return 'Codex 的 hooks.json 不在，Codex 的 hook 事件（回合結束、需要回應）就送不到 agents-manager。有「可自動補」就用補齊；「需手動」照提示自己加。'
  if (id.startsWith('codex.config.toml:')) return 'config.toml 缺這個鍵；agents-manager 依賴它（approval_policy 決定審核，features.hooks 讓 hook 生效）。有「可自動補」就用補齊。'
  if (id === 'grok.config.toml') return '沒有 ~/.grok/config.toml，Grok 用預設設定。只有要用 Grok 時才需要處理。'
  if (id === 'herdr.config.toml') return '沒有 herdr 的 config.toml，herdr 用預設設定。只有要自訂 herdr 時才需要補。'
  return null
}
