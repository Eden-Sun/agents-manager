// 英文字典的集中處。每個區塊一個檔（各自 `export default` 一份 Dict），並行抽字串的人各改各的檔、不碰這支。
// 區塊與檔案歸屬見 lib/i18n/areas.ts；區塊檔需要拆小時，在自己的區塊檔裡 import 子檔，不要改這裡。
import type { Dict } from '../../lib/i18n/types'
import { mergeDicts } from '../../lib/i18n/translate'
import bots from './bots'
import chat from './chat'
import common from './common'
import core from './core'
import hosts from './hosts'
import updates from './updates'

export const EN_PARTS: Readonly<Record<string, Dict>> = { common, chat, bots, hosts, updates, core }
export const { dict: EN, conflicts: EN_CONFLICTS } = mergeDicts(EN_PARTS)
