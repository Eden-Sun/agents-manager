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
import chatAgy from './chat-agy'
import botsAgy from './bots-agy'
import hostsAgy from './hosts-agy'
import updatesAgy from './updates-agy'
import coreAgy from './core-agy'

export const EN_PARTS: Readonly<Record<string, Dict>> = {
  common,
  chat,
  bots,
  hosts,
  updates,
  core,
  // agy 並行分擔（#1932）：各自一檔，避免與區塊主檔衝突
  'chat-agy': chatAgy,
  'bots-agy': botsAgy,
  'hosts-agy': hostsAgy,
  'updates-agy': updatesAgy,
  'core-agy': coreAgy,
}
export const { dict: EN, conflicts: EN_CONFLICTS } = mergeDicts(EN_PARTS)
