/**
 * 「交給 AGM」建的是任務，任務只帶文字（`startMission` 沒有附件欄位）。以前照樣能加附件，送出時附件靜靜丟掉，
 * 通知卻說已交給 AGM；執行者看不到圖，附件還掛在輸入框，使用者以為一起送了（review3 c1 M7）。
 * 帶著附件就不送，講清楚要怎麼做；回 `null` 才送得出去。
 */
export function agmAttachmentBlock(toAgm: boolean, attachments: number): string | null {
  if (!toAgm || attachments === 0) return null
  return `交給 AGM 的任務還帶不了附件（${attachments} 個）：先移除附件，或取消「交給 AGM」直接送給 bot。`
}
