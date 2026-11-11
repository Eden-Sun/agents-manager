import { useState } from 'react'
import { copyText } from '../lib/copyText'
import { useT } from '../lib/i18n'
import { toolInstallInfo } from '../lib/toolInstall'

/**
 * 缺工具的安裝引導：依 OS 列指令（可複製）、官方連結、裝完重新檢查的按鈕。
 * 只給指令，不自動執行。裝完要讓「登入 shell」的 PATH 找得到，這點一定要說。
 */
export function ToolInstallGuide({ tool, os, onRecheck }: { tool: string; os: string | null; onRecheck: () => Promise<unknown> }) {
  const t = useT()
  const [busy, setBusy] = useState(false)
  const info = toolInstallInfo(tool, os)

  const recheck = () => {
    if (busy) return
    setBusy(true)
    void Promise.resolve()
      .then(onRecheck)
      .catch(() => undefined)
      .finally(() => setBusy(false))
  }

  return (
    <span className="tool-install-guide">
      {info && info.steps.length > 0 ? (
        info.steps.map((s) => (
          <span key={s.command} className="tool-install-step">
            <span className="tool-install-os">{s.label}</span>
            <CopyCommand command={s.command} />
          </span>
        ))
      ) : (
        <span className="tool-install-step">{t('這個工具沒有可自動核對的安裝指令，請依它的安裝說明裝。')}</span>
      )}
      <span className="tool-install-note">
        {t('裝到登入 shell 的 PATH 上才找得到（例如 brew 的 bin 目錄或 ~/.local/bin，並確認 ~/.zprofile 或 ~/.profile 有加進 PATH）。')}
      </span>
      <span className="tool-install-actions">
        {info?.link ? (
          <a href={info.link} target="_blank" rel="noreferrer">
            {t('官方說明')}
          </a>
        ) : null}
        <button type="button" className="mini-btn" disabled={busy} onClick={recheck}>
          {busy ? t('檢查中…') : t('重新檢查')}
        </button>
      </span>
    </span>
  )
}

function CopyCommand({ command }: { command: string }) {
  const t = useT()
  const [state, setState] = useState<'idle' | 'ok' | 'fail'>('idle')
  return (
    <span className="tool-install-cmd">
      <code>{command}</code>
      <button
        type="button"
        className="mini-btn"
        onClick={() => {
          void copyText(command).then((ok) => setState(ok ? 'ok' : 'fail'))
        }}
      >
        {state === 'ok' ? t('已複製') : state === 'fail' ? t('複製失敗，請手動選取') : t('複製')}
      </button>
    </span>
  )
}
