
export interface ToolInstallStep {
  label: string
  command: string
}

export interface ToolInstallInfo {
  steps: ToolInstallStep[]
  link: string | null
}

const INSTALL: Record<string, { mac?: string; linux?: string; link: string | null }> = {
  bun: { mac: 'brew install oven-sh/bun/bun', linux: 'curl -fsSL https://bun.sh/install | bash', link: 'https://bun.sh' },
  gh: { mac: 'brew install gh', linux: 'sudo apt install gh', link: 'https://cli.github.com' },
  jq: { mac: 'brew install jq', linux: 'sudo apt install jq', link: 'https://jqlang.github.io/jq/download/' },
  zsh: { mac: 'brew install zsh', linux: 'sudo apt install zsh', link: 'https://www.zsh.org/' },
  herdr: { mac: 'brew install herdr', linux: 'curl -fsSL https://herdr.dev/install.sh | sh', link: 'https://herdr.dev' },
  rtk: { link: null },
}

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

export function baselineWarnWhy(id: string): string | null {
  if (id === 'codex.config.toml') return 'Codex 的 config.toml 不在，會用內建預設跑；agents-manager 需要的設定不會生效。有「可自動補」就用一致性補齊；「需手動」照提示自己加。'
  if (id === 'codex.hooks.json') return 'Codex 的 hooks.json 不在，Codex 的 hook 事件（回合結束、需要回應）就送不到 agents-manager。有「可自動補」就用補齊；「需手動」照提示自己加。'
  if (id.startsWith('codex.config.toml:')) return 'config.toml 缺這個鍵；agents-manager 依賴它（approval_policy 決定審核，features.hooks 讓 hook 生效）。有「可自動補」就用補齊。'
  if (id === 'grok.config.toml') return '沒有 ~/.grok/config.toml，Grok 用預設設定。只有要用 Grok 時才需要處理。'
  if (id === 'herdr.config.toml') return '沒有 herdr 的 config.toml，herdr 用預設設定。只有要自訂 herdr 時才需要補。'
  return null
}
