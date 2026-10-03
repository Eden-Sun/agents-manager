import type { BotKind, ShareFolderIn } from '../api/types'

/** 能不能建成分享用（受限）的 bot：daemon 只做了 claude、而且資料夾在本機（SPEC §20）。null＝可以。 */
export function shareProfileBlocked(kind: BotKind, host: string): string | null {
  if (kind !== 'claude') return '分享用的受限 bot 目前只支援 claude'
  if (host && host !== 'local') return '分享用的受限 bot 只能建在本機的專案'
  return null
}

/** 建 bot 表單裡「它的資料夾」的草稿：新資料夾（名字，空的＝用 bot 名）或既有資料夾（絕對路徑）。 */
export interface ShareFolderDraft {
  mode: 'new' | 'existing'
  name: string
  path: string
}

export const EMPTY_SHARE_FOLDER: ShareFolderDraft = { mode: 'new', name: '', path: '' }

/** 同 daemon `share::folder::check_new_name`。 */
const FOLDER_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/

/** 草稿 → `share_folder`；填得不對回 `{error}`（表單擋下送出並顯示）。 */
export function shareFolderInput(d: ShareFolderDraft, botName: string): { value: ShareFolderIn } | { error: string } {
  if (d.mode === 'existing') {
    const path = d.path.trim()
    if (!path.startsWith('/')) return { error: '既有資料夾要給絕對路徑（/ 開頭），或按「瀏覽…」挑一個' }
    return { value: { kind: 'existing', path } }
  }
  const name = d.name.trim() || botName.trim()
  if (!FOLDER_NAME.test(name)) return { error: '資料夾名稱只能用英數與 . _ -，英數開頭，最多 64 字' }
  return { value: { kind: 'new', name } }
}
