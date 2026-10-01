/** AGM 面板的標籤表（獨立成檔：元件檔只能 export 元件，見 fastRefreshExports.test）。 */

/**
 * 交辦的生命週期。`awaiting_review` 是這裡最重要的一格：回合跑完只到這裡，AGM 驗收過
 * 才會變 `completed`。把它畫成「已完成」就是在替沒人看過的工作背書。
 */
export const ASSIGN_LABEL: Record<string, string> = {
  queued: '待送出',
  delivered: '已送達',
  unknown: '送達未知',
  awaiting_review: '等驗收',
  blocked: '阻塞中',
  quota_blocked: '等額度',
  completed: '已驗收',
  failed: '失敗',
  cancelled: '已取消',
  superseded: '已接續',
}

export const INCIDENT_LABEL: Record<string, string> = {
  host_disconnected: '主機斷線',
  bot_stopped: 'bot 該開著卻停了',
  assignment_stalled: '交辦卡住沒進度',
  assignment_undelivered: '交辦一直送不出去',
  notify_exhausted: '通知送不出去',
  remote_entry: '遠端入口異常',
  approval_stalled: '核准等太久沒人裁示',
  role_unavailable: 'AGM 角色不可用',
  responder_undeliverable: '協調者收不到通知',
  inbox_classify_failing: '事件分類器壞了',
  remote_shim_stale: '遠端 shim 過期',
  remote_spool_stuck: '遠端 spool 卡住',
}
