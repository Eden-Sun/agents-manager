//! 只讀 ZIP central directory 判定 OOXML 檔；不解壓任何成員。

use std::collections::HashMap;

const EOCD: &[u8; 4] = b"PK\x05\x06";
const CENTRAL: &[u8; 4] = b"PK\x01\x02";
const LOCAL: &[u8; 4] = b"PK\x03\x04";
const DATA_DESCRIPTOR: u32 = 0x0807_4b50;
const MAX_ENTRIES: usize = 4096;
const MAX_CENTRAL_DIRECTORY: usize = 1024 * 1024;
const MAX_UNCOMPRESSED: u64 = 512 * 1024 * 1024;
const ALLOWED_FLAGS: u16 = (1 << 1) | (1 << 2) | (1 << 3) | (1 << 11);

pub(super) fn valid_for(data: &[u8], required_root: &[u8]) -> bool {
    let Some(entries) = parse_directory(data) else {
        return false;
    };
    [
        b"[Content_Types].xml".as_slice(),
        b"_rels/.rels",
        required_root,
    ]
    .iter()
    .all(|name| entries.get(&name.to_vec()) == Some(&true))
}

fn parse_directory(data: &[u8]) -> Option<HashMap<Vec<u8>, bool>> {
    if data.len() < 22 || data.len() > super::MAX_UPLOAD {
        return None;
    }
    let eocd = find_eocd(data)?;
    let disk = u16_at(data, eocd + 4)?;
    let central_disk = u16_at(data, eocd + 6)?;
    let entries_on_disk = usize::from(u16_at(data, eocd + 8)?);
    let entry_count = usize::from(u16_at(data, eocd + 10)?);
    let central_size = usize::try_from(u32_at(data, eocd + 12)?).ok()?;
    let central_offset = usize::try_from(u32_at(data, eocd + 16)?).ok()?;
    if disk != 0
        || central_disk != 0
        || entries_on_disk != entry_count
        || entry_count == 0
        || entry_count > MAX_ENTRIES
        || entry_count == u16::MAX as usize
    {
        return None;
    }
    let central_end = central_offset.checked_add(central_size)?;
    if central_size > MAX_CENTRAL_DIRECTORY || central_end != eocd {
        return None;
    }

    let mut cursor = central_offset;
    let mut entries = HashMap::with_capacity(entry_count);
    let mut local_ranges = Vec::with_capacity(entry_count);
    let mut uncompressed_total = 0_u64;
    for _ in 0..entry_count {
        if data.get(cursor..cursor.checked_add(4)?)? != CENTRAL {
            return None;
        }
        let made_by = u16_at(data, cursor + 4)?;
        let flags = u16_at(data, cursor + 8)?;
        let method = u16_at(data, cursor + 10)?;
        let crc = u32_at(data, cursor + 16)?;
        let compressed_size = u32_at(data, cursor + 20)?;
        let uncompressed_size = u32_at(data, cursor + 24)?;
        let name_len = usize::from(u16_at(data, cursor + 28)?);
        let extra_len = usize::from(u16_at(data, cursor + 30)?);
        let comment_len = usize::from(u16_at(data, cursor + 32)?);
        let disk_start = u16_at(data, cursor + 34)?;
        let external_attrs = u32_at(data, cursor + 38)?;
        let local_offset = u32_at(data, cursor + 42)?;
        if name_len == 0
            || disk_start != 0
            || name_len > 4096
            || flags & !ALLOWED_FLAGS != 0
            || (method == 0 && flags & 0x0006 != 0)
            || !matches!(method, 0 | 8)
        {
            return None;
        }
        if [compressed_size, uncompressed_size, local_offset].contains(&u32::MAX)
            || disk_start == u16::MAX
        {
            return None;
        }
        let record_end = cursor
            .checked_add(46)?
            .checked_add(name_len)?
            .checked_add(extra_len)?
            .checked_add(comment_len)?;
        if record_end > central_end {
            return None;
        }
        let name_start = cursor + 46;
        let name = data.get(name_start..name_start.checked_add(name_len)?)?;
        if !safe_name(name) || (flags & (1 << 11) != 0 && std::str::from_utf8(name).is_err()) {
            return None;
        }
        let extra_start = name_start + name_len;
        let extra = data.get(extra_start..extra_start.checked_add(extra_len)?)?;
        if !valid_extra(extra) {
            return None;
        }
        let uncompressed_size = u64::from(uncompressed_size);
        uncompressed_total = uncompressed_total.checked_add(uncompressed_size)?;
        if uncompressed_total > MAX_UNCOMPRESSED
            || (method == 0 && compressed_size != uncompressed_size as u32)
        {
            return None;
        }

        let host_os = made_by >> 8;
        let mode = (external_attrs >> 16) as u16;
        let file_type = mode & 0o170000;
        if host_os == 3 && !matches!(file_type, 0 | 0o040000 | 0o100000) {
            return None;
        }
        let is_directory = name.ends_with(b"/")
            || external_attrs & 0x10 != 0
            || (host_os == 3 && file_type == 0o040000);
        let is_file = !is_directory && uncompressed_size != 0;

        let local_offset = usize::try_from(local_offset).ok()?;
        let local_end = validate_local(
            data,
            central_offset,
            local_offset,
            name,
            flags,
            method,
            crc,
            compressed_size,
            uncompressed_size as u32,
        )?;
        local_ranges.push((local_offset, local_end));
        if entries.insert(name.to_vec(), is_file).is_some() {
            return None;
        }
        cursor = record_end;
    }
    if cursor != central_end {
        return None;
    }
    local_ranges.sort_unstable();
    if local_ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return None;
    }
    Some(entries)
}

fn find_eocd(data: &[u8]) -> Option<usize> {
    let last_start = data.len().checked_sub(22)?;
    let first_start = last_start.saturating_sub(u16::MAX as usize);
    (first_start..=last_start).rev().find(|&at| {
        data.get(at..at + 4) == Some(EOCD)
            && u16_at(data, at + 20).is_some_and(|comment_len| {
                at.checked_add(22 + usize::from(comment_len)) == Some(data.len())
            })
    })
}

fn validate_local(
    data: &[u8],
    central_offset: usize,
    local_offset: usize,
    name: &[u8],
    flags: u16,
    method: u16,
    crc: u32,
    compressed_size: u32,
    uncompressed_size: u32,
) -> Option<usize> {
    if local_offset.checked_add(30)? > central_offset
        || data.get(local_offset..local_offset.checked_add(4)?)? != LOCAL
    {
        return None;
    }
    let local_flags = u16_at(data, local_offset + 6)?;
    let local_method = u16_at(data, local_offset + 8)?;
    let local_crc = u32_at(data, local_offset + 14)?;
    let local_compressed = u32_at(data, local_offset + 18)?;
    let local_uncompressed = u32_at(data, local_offset + 22)?;
    let name_len = usize::from(u16_at(data, local_offset + 26)?);
    let extra_len = usize::from(u16_at(data, local_offset + 28)?);
    let name_start = local_offset + 30;
    let local_name = data.get(name_start..name_start.checked_add(name_len)?)?;
    let extra_start = name_start.checked_add(name_len)?;
    let extra = data.get(extra_start..extra_start.checked_add(extra_len)?)?;
    if local_flags != flags || local_method != method || local_name != name || !valid_extra(extra) {
        return None;
    }
    if flags & (1 << 3) == 0 {
        if local_crc != crc
            || local_compressed != compressed_size
            || local_uncompressed != uncompressed_size
        {
            return None;
        }
    } else if (local_crc != 0 && local_crc != crc)
        || (local_compressed != 0 && local_compressed != compressed_size)
        || (local_uncompressed != 0 && local_uncompressed != uncompressed_size)
    {
        return None;
    }
    let payload_start = extra_start.checked_add(extra_len)?;
    let payload_end = payload_start.checked_add(usize::try_from(compressed_size).ok()?)?;
    if payload_end > central_offset {
        return None;
    }
    if flags & (1 << 3) == 0 {
        return Some(payload_end);
    }
    let signed = u32_at(data, payload_end)? == DATA_DESCRIPTOR;
    let descriptor = payload_end.checked_add(if signed { 4 } else { 0 })?;
    if u32_at(data, descriptor)? != crc
        || u32_at(data, descriptor + 4)? != compressed_size
        || u32_at(data, descriptor + 8)? != uncompressed_size
    {
        return None;
    }
    let end = descriptor.checked_add(12)?;
    (end <= central_offset).then_some(end)
}

fn valid_extra(mut bytes: &[u8]) -> bool {
    while !bytes.is_empty() {
        if bytes.len() < 4 {
            return false;
        }
        let tag = u16_at(bytes, 0).unwrap_or(u16::MAX);
        let len = usize::from(u16_at(bytes, 2).unwrap_or(u16::MAX));
        if tag == 0x0001 || bytes.len() < 4 + len {
            return false;
        }
        bytes = &bytes[4 + len..];
    }
    true
}

fn safe_name(name: &[u8]) -> bool {
    !name.is_empty()
        && !name.starts_with(b"/")
        && !name.contains(&b'\\')
        && !name.contains(&b':')
        && !name.iter().any(|byte| *byte == 0 || *byte < 0x20)
        && name
            .split(|byte| *byte == b'/')
            .all(|part| part != b".." && part != b".")
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        data.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        data.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}
