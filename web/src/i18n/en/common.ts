import type { Dict } from '../../lib/i18n/types'

// 跨區塊共用的字（取消、儲存、確定…）。各區塊也可以各自寫一份「英文完全相同」的（不算衝突），所以不必先來這裡登記；
// 這裡只放確定每一區都會用到、想統一的字。字典的 key 一定要真的出現在原始碼裡（i18nDict.test.ts 會檢查）。
export default {} satisfies Dict
