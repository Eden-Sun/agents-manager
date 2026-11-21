import type { BotKind, ShareFolderIn, ShareProfile } from '../api/types'
import { t } from './i18n'

export function shareProfileBlocked(kind: BotKind): string | null {
  if (kind !== 'claude') return t('分享用的 bot 目前只支援 claude')
  return null
}

export const shareIcon = (p: ShareProfile | null | undefined): string => (p === 'trusted' ? '🔓' : '🔗')

export interface ShareFolderDraft {
  mode: 'new' | 'existing'
  name: string
  path: string
}

export const EMPTY_SHARE_FOLDER: ShareFolderDraft = { mode: 'new', name: '', path: '' }

const FOLDER_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/

export function shareFolderInput(d: ShareFolderDraft, botName: string): { value: ShareFolderIn } | { error: string } {
  if (d.mode === 'existing') {
    const path = d.path.trim()
    if (!path.startsWith('/')) return { error: t('既有資料夾要給絕對路徑（/ 開頭），或按「瀏覽…」挑一個') }
    return { value: { kind: 'existing', path } }
  }
  const name = d.name.trim() || botName.trim()
  if (!FOLDER_NAME.test(name)) return { error: t('資料夾名稱只能用英數與 . _ -，英數開頭，最多 64 字') }
  return { value: { kind: 'new', name } }
}

export function shareCreateFields(
  profile: ShareProfile | null,
  folderIn: { value: ShareFolderIn } | { error: string } | null,
):
  | { share_profile: 'restricted'; share_folder: ShareFolderIn; auto_approve: false }
  | { share_profile: 'trusted'; share_folder: ShareFolderIn; confirm_trusted: true; auto_approve: true }
  | { auto_approve: true } {
  if (folderIn && 'value' in folderIn && profile === 'restricted') return { share_profile: 'restricted', share_folder: folderIn.value, auto_approve: false }
  if (folderIn && 'value' in folderIn && profile === 'trusted')
    return { share_profile: 'trusted', share_folder: folderIn.value, confirm_trusted: true, auto_approve: true }
  return { auto_approve: true }
}
