import { msg } from './i18n'

export const ASSIGN_LABEL: Record<string, string> = {
  queued: msg('待送出'),
  delivered: msg('已送達'),
  unknown: msg('送達未知'),
  awaiting_review: msg('等驗收'),
  blocked: msg('阻塞中'),
  quota_blocked: msg('等額度'),
  completed: msg('已驗收'),
  failed: msg('失敗'),
  cancelled: msg('已取消'),
  superseded: msg('已接續'),
}

export const INCIDENT_LABEL: Record<string, string> = {
  host_disconnected: msg('主機斷線'),
  bot_stopped: msg('bot 該開著卻停了'),
  assignment_stalled: msg('交辦卡住沒進度'),
  assignment_undelivered: msg('交辦一直送不出去'),
  notify_exhausted: msg('通知送不出去'),
  remote_entry: msg('遠端入口異常'),
  approval_stalled: msg('核准等太久沒人裁示'),
  role_unavailable: msg('AGM 角色不可用'),
  responder_undeliverable: msg('協調者收不到通知'),
  inbox_classify_failing: msg('事件分類器壞了'),
  remote_shim_stale: msg('遠端 shim 過期'),
  remote_spool_stuck: msg('遠端 spool 卡住'),
}
