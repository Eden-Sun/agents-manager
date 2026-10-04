import { useState } from 'react'
import type { BotKind, ShareProfile } from '../api/types'
import { shareFolderInput, shareProfileBlocked, type ShareFolderDraft } from '../lib/shareProfile'
import { ConfirmDialog } from './ConfirmDialog'
import { DirPicker } from './DirPicker'
import './botShare.css'

const RESTRICTED_HINT =
  '只能讀寫下面這個資料夾（與它自己的 outbox）；不能跑指令（沒有 Bash）、不能抓網頁（沒有 WebFetch，可以用 WebSearch）、不能開子 agent、不帶你的憑證。資料夾裡的 CLAUDE.md／AGENTS.md 與 memory/ 會當成它的指示與記憶。模型沒選＝最新的 Opus。建好後可在設定裡開分享連結，之後不能改回一般 bot。'
const TRUSTED_HINT =
  '跟一般 bot 一樣的權限：在下面這個資料夾（通常是專案目錄）裡跑指令、build、測試、git、dev server 都會真的動到東西，權限照 bot 設定。對方只看得到他自己和 bot 的對話。建好後可開分享連結，之後不能改回一般 bot。'

/**
 * 建 bot 表單的「用途」（SPEC §20）：不分享／分享用（受限）／信任分享，三選一；建好之後不能切換，要分享就新建一顆。
 * 受限：選它的資料夾（新資料夾 `~/shared-bots/<名稱>` 或本機既有資料夾），那就是它能碰的全部範圍。
 * 信任分享：只用既有資料夾（預設專案目錄），選的當下要在確認框裡勾「只分享給絕對信任的人」才算數。
 */
export function ShareProfileField({
  kind,
  host,
  value,
  onChange,
  botName,
  folder,
  onFolder,
  projectPath,
}: {
  kind: BotKind
  host: string
  value: ShareProfile | null
  onChange: (v: ShareProfile | null) => void
  botName: string
  folder: ShareFolderDraft
  onFolder: (f: ShareFolderDraft) => void
  /** 信任分享的預設資料夾。 */
  projectPath?: string
}) {
  const blocked = shareProfileBlocked(kind, host)
  const [browsing, setBrowsing] = useState(false)
  const [asking, setAsking] = useState(false)
  const [ack, setAck] = useState(false)
  const profile = blocked ? null : value
  const on = profile !== null
  const trusted = profile === 'trusted'
  const checked = on ? shareFolderInput(folder, botName) : null
  const pick = (p: ShareProfile | null) => {
    if (p === 'trusted') {
      setAck(false)
      setAsking(true)
      return
    }
    onChange(p)
  }
  return (
    <div className="field share-profile-field">
      <span>用途</span>
      <div className="share-profile-choices" role="radiogroup" aria-label="用途">
        <label>
          <input type="radio" name="share-profile" checked={!on} onChange={() => pick(null)} />
          不分享
        </label>
        <label title={blocked ?? undefined}>
          <input type="radio" name="share-profile" checked={profile === 'restricted'} disabled={Boolean(blocked)} onChange={() => pick('restricted')} />
          分享用（受限）
        </label>
        <label title={blocked ?? undefined} className="share-profile-trusted">
          <input type="radio" name="share-profile" checked={trusted} disabled={Boolean(blocked)} onChange={() => pick('trusted')} />
          🔓 信任分享
        </label>
      </div>
      <span className="hint">{blocked ?? (profile === 'restricted' ? RESTRICTED_HINT : trusted ? TRUSTED_HINT : '要把這顆 bot 分享給外部使用者時才選。')}</span>
      <ConfirmDialog
        open={asking}
        title="建立信任分享的 bot？"
        body={
          <>
            <p>
              <strong>拿到連結的人可以透過它操作這台機器上的任何東西</strong>：跑指令、改檔案、build、git、開 dev server，權限跟你自己的 bot 一樣。只分享給絕對信任的人。
            </p>
            <label className="share-trusted-ack">
              <input type="checkbox" checked={ack} onChange={(e) => setAck(e.target.checked)} />
              我了解，只會分享給絕對信任的人
            </label>
          </>
        }
        confirmLabel="建立信任分享"
        danger
        confirmDisabled={!ack}
        onCancel={() => setAsking(false)}
        onConfirm={() => {
          setAsking(false)
          // 信任分享只用既有資料夾；沒填過就帶專案目錄。
          onFolder({ ...folder, mode: 'existing', path: folder.path || projectPath || '' })
          onChange('trusted')
        }}
      />
      {on ? (
        <div className="share-folder" role="radiogroup" aria-label="它的資料夾">
          {trusted ? null : (
            <label>
              <input type="radio" name="share-folder" checked={folder.mode === 'new'} onChange={() => onFolder({ ...folder, mode: 'new' })} />
              開一個新資料夾
            </label>
          )}
          {folder.mode === 'new' && !trusted ? (
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
            <input type="radio" name="share-folder" checked={folder.mode === 'existing' || trusted} onChange={() => onFolder({ ...folder, mode: 'existing' })} />
            {trusted ? '它工作的資料夾（cwd）' : '用一個既有的資料夾'}
          </label>
          {folder.mode === 'existing' || trusted ? (
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
                {trusted
                  ? '⚠ 拿到連結的人可以透過它操作這台機器上的任何東西，不只這個資料夾。只分享給絕對信任的人。'
                  : '⚠ 這個資料夾裡的所有檔案（含 .env 之類）拿到連結的人都可能透過 bot 讀到。常見的金鑰檔（.env*、*.pem、*.key、id_*）會再擋一層，但不要選放著秘密的資料夾。'}
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
