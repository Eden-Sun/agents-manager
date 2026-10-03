import { useState } from 'react'
import type { BotKind } from '../api/types'
import { shareFolderInput, shareProfileBlocked, type ShareFolderDraft } from '../lib/shareProfile'
import { DirPicker } from './DirPicker'
import './botShare.css'

/**
 * 建 bot 表單的「分享用（受限）」（SPEC §20）：建好之後不能切換，要分享就新建一顆。
 * 勾了之後選它的資料夾——那個資料夾就是它能碰的全部範圍：新資料夾（`~/shared-bots/<名稱>`）或本機既有資料夾。
 */
export function ShareProfileField({
  kind,
  host,
  value,
  onChange,
  botName,
  folder,
  onFolder,
}: {
  kind: BotKind
  host: string
  value: boolean
  onChange: (v: boolean) => void
  botName: string
  folder: ShareFolderDraft
  onFolder: (f: ShareFolderDraft) => void
}) {
  const blocked = shareProfileBlocked(kind, host)
  const [browsing, setBrowsing] = useState(false)
  const on = value && !blocked
  const checked = on ? shareFolderInput(folder, botName) : null
  return (
    <div className="field share-profile-field">
      <span>用途</span>
      <label title={blocked ?? undefined}>
        <input type="checkbox" checked={on} disabled={Boolean(blocked)} onChange={(e) => onChange(e.target.checked)} />
        分享用（受限）
      </label>
      <span className="hint">
        {blocked ??
          (on
            ? '只能讀寫下面這個資料夾（與它自己的 outbox）；不能跑指令（沒有 Bash）、不能抓網頁（沒有 WebFetch，可以用 WebSearch）、不能開子 agent、不帶你的憑證。資料夾裡的 CLAUDE.md／AGENTS.md 與 memory/ 會當成它的指示與記憶。模型沒選＝最新的 Opus。建好後可在設定裡開分享連結，之後不能改回一般 bot。'
            : '要把這顆 bot 分享給外部使用者時才勾。')}
      </span>
      {on ? (
        <div className="share-folder" role="radiogroup" aria-label="它的資料夾">
          <label>
            <input type="radio" name="share-folder" checked={folder.mode === 'new'} onChange={() => onFolder({ ...folder, mode: 'new' })} />
            開一個新資料夾
          </label>
          {folder.mode === 'new' ? (
            <div className="share-folder-row">
              <span className="share-folder-prefix">~/shared-bots/</span>
              <input
                type="text"
                value={folder.name}
                placeholder={botName || '名稱'}
                spellCheck={false}
                aria-label="新資料夾名稱"
                onChange={(e) => onFolder({ ...folder, name: e.target.value })}
              />
            </div>
          ) : null}
          <label>
            <input type="radio" name="share-folder" checked={folder.mode === 'existing'} onChange={() => onFolder({ ...folder, mode: 'existing' })} />
            用一個既有的資料夾
          </label>
          {folder.mode === 'existing' ? (
            <>
              <div className="share-folder-row">
                <input
                  type="text"
                  value={folder.path}
                  placeholder="/home/you/project/docs"
                  spellCheck={false}
                  aria-label="既有資料夾路徑"
                  onChange={(e) => onFolder({ ...folder, path: e.target.value })}
                />
                <button type="button" className="btn" onClick={() => setBrowsing(true)}>
                  瀏覽…
                </button>
              </div>
              <p className="share-folder-warn" role="alert">
                ⚠ 這個資料夾裡的所有檔案（含 .env 之類）拿到連結的人都可能透過 bot 讀到。常見的金鑰檔（.env*、*.pem、*.key、id_*）會再擋一層，但不要選放著秘密的資料夾。
              </p>
            </>
          ) : null}
          {checked && 'error' in checked ? <span className="hint share-folder-err">{checked.error}</span> : null}
          {browsing ? (
            <DirPicker
              initial={folder.path || undefined}
              onPick={(p) => {
                onFolder({ ...folder, mode: 'existing', path: p })
                setBrowsing(false)
              }}
              onCancel={() => setBrowsing(false)}
            />
          ) : null}
        </div>
      ) : null}
    </div>
  )
}
