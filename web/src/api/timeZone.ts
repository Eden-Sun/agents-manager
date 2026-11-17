import { rawTransport } from './index'

/** 使用者顯示時區（daemon 半：`/api/settings/time-zone`）。 */
export interface TimeZoneSettingsValue {
  time_zone: string
  utc_offset: string
}

export async function getTimeZone(): Promise<TimeZoneSettingsValue> {
  return (await rawTransport.request('GET', '/settings/time-zone')) as TimeZoneSettingsValue
}

/** 不合法的名稱 daemon 回 400 `{"error":"bad_time_zone"}`（呼叫端用 `ApiError.status` 辨認）。 */
export async function setTimeZone(zone: string): Promise<TimeZoneSettingsValue> {
  return (await rawTransport.request('PUT', '/settings/time-zone', { time_zone: zone })) as TimeZoneSettingsValue
}
