export const SHARE_IMAGE_REQUEUE_MS = 1500
/** 分享下載的上限（daemon `outbox::MAX_BYTES`，超過回 413 `too_large`）。這個大小以上的圖重試不會好，直接說太大了、不發請求。 */
export const SHARE_IMAGE_MAX_BYTES = 64 * 1024 * 1024

/** `<img>` 載入失敗（下載名額每個分享同時只有 2 個、滿了 429；或網路一閃）先自己重試，用完才算這一版載不出來。 */
export const IMG_RETRIES = 3
export const IMG_RETRY_MS = 1500
