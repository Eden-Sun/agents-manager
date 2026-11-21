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
  'chat-agy': chatAgy,
  'bots-agy': botsAgy,
  'hosts-agy': hostsAgy,
  'updates-agy': updatesAgy,
  'core-agy': coreAgy,
}
export const { dict: EN, conflicts: EN_CONFLICTS } = mergeDicts(EN_PARTS)
