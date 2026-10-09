//! 遠端檔案原語與傳輸（SPEC §20、remote-share-design §3）。
//!
//! 所有遠端檔案動作走同一個模組：POSIX sh 腳本、逐段 `cd -P` 不跟符號連結、
//! 開檔 `exec 3< "$F"` 後以 `am_same` 與 `am_singlelink` 驗證身分，
//! 輸出使用長度框（`AM_RFS1` 協定）傳回。任何一步對不上或出現不確定一律 fail closed。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use am_base::hosts::{sh_quote, HostConn, SshStream};
use am_base::outbox::{content_is_withheld, withheld_name, ShareFileError};
use serde_json::{json, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::share::budget;
use crate::share::site::RemoteSite;

pub const RFS_VERSION_TAG: &str = "AM_RFS1";
pub const RFS_DONE_TAG: &str = "AM_RFS_DONE";

/// 每個主機的分享 SSH 同時連線上限（避免耗盡 ControlMaster 的 MaxSessions）。
pub const HOST_SHARE_SLOTS: usize = 4;

/// 遠端操作的各種超時規定（§3.1）。
pub const TIMEOUT_QUICK: Duration = Duration::from_secs(30);
pub const TIMEOUT_UPLOAD: Duration = Duration::from_secs(120);
pub const TIMEOUT_MAINTAIN: Duration = Duration::from_secs(60);
pub const TIMEOUT_STREAM_HEAD: Duration = Duration::from_secs(30);
pub const TIMEOUT_STREAM_IDLE: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFolder {
    pub physical: String,
    pub home_physical: String,
    pub root_physical: String,
    pub owned_by_me: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RfsError {
    #[error("folder already exists")]
    Exists,
    #[error("unsafe folder: {0}")]
    Unsafe(String),
    #[error("untrusted path or file")]
    Untrusted,
    #[error("not found")]
    NotFound,
    #[error("remote service unavailable or timeout")]
    Unavailable,
    #[error("other error: {0}")]
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InboxError {
    #[error("inbox is full")]
    Full,
    #[error("inbox write unavailable")]
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadWithStat {
    pub name: String,
    pub data: Vec<u8>,
    pub ino: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhotoStat {
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i128,
}

/// 遠端 outbox 串流下載物件。
pub struct RemoteFile {
    pub len: u64,
    pub head: Vec<u8>,
    pub body: SshStream,
    pub permit: OwnedSemaphorePermit,
}

// ───────────────────── 主機 Semaphore 名額管理 ─────────────────────

static HOST_SLOTS: std::sync::Mutex<Option<HashMap<String, Arc<Semaphore>>>> = std::sync::Mutex::new(None);

pub fn host_slot(host: &str) -> Arc<Semaphore> {
    let mut guard = HOST_SLOTS.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    map.entry(host.to_string())
        .or_insert_with(|| Arc::new(Semaphore::new(HOST_SHARE_SLOTS)))
        .clone()
}

pub async fn acquire_slot(host: &str, timeout: Duration) -> Result<OwnedSemaphorePermit, RfsError> {
    let sem = host_slot(host);
    match tokio::time::timeout(timeout, sem.acquire_owned()).await {
        Ok(Ok(permit)) => Ok(permit),
        _ => Err(RfsError::Unavailable),
    }
}

// ───────────────────── RFS1 協定 Frame 解析 ─────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RfsFrame {
    pub tag: String,
    pub data: Vec<u8>,
}

/// 解析遠端腳本以 `AM_RFS1` 開頭、`AM_RFS_DONE` 結尾的長度框。
/// 遇到格式錯誤、長度不對、或未在 `allowed_tags`（以及 `ERR`）宣告的標籤，立即 fail-closed 回 Err。
pub fn parse_rfs_frames(input: &[u8], allowed_tags: &[&str]) -> Result<Vec<RfsFrame>, RfsError> {
    let mut cursor = input;
    // 檢查開頭版本標籤
    let header_prefix = format!("{RFS_VERSION_TAG}\n");
    if cursor.starts_with(header_prefix.as_bytes()) {
        cursor = &cursor[header_prefix.len()..];
    } else if cursor.starts_with(format!("{RFS_VERSION_TAG}\r\n").as_bytes()) {
        cursor = &cursor[RFS_VERSION_TAG.len() + 2..];
    } else {
        return Err(RfsError::Unavailable);
    }

    let mut frames = Vec::new();
    loop {
        // 檢查是否以 AM_RFS_DONE 結束
        if cursor.starts_with(RFS_DONE_TAG.as_bytes()) {
            let rest = &cursor[RFS_DONE_TAG.len()..];
            if rest.is_empty() || rest == b"\n" || rest == b"\r\n" {
                return Ok(frames);
            }
        }

        if cursor.is_empty() {
            // 沒有 AM_RFS_DONE 結尾即結束 → 不完整作廢
            return Err(RfsError::Unavailable);
        }

        // 讀取行頭 TAG len\n
        let line_end = cursor.iter().position(|&b| b == b'\n').ok_or(RfsError::Unavailable)?;
        let line_bytes = &cursor[..line_end];
        let line_str = std::str::from_utf8(line_bytes).map_err(|_| RfsError::Unavailable)?.trim_end_matches('\r');
        cursor = &cursor[line_end + 1..];

        if line_str == RFS_DONE_TAG {
            if cursor.is_empty() || cursor == b"\n" || cursor == b"\r\n" {
                return Ok(frames);
            }
            return Err(RfsError::Unavailable);
        }

        let mut parts = line_str.split_whitespace();
        let tag = parts.next().ok_or(RfsError::Unavailable)?;
        let len_str = parts.next().ok_or(RfsError::Unavailable)?;
        if parts.next().is_some() {
            return Err(RfsError::Unavailable);
        }

        let len: usize = len_str.parse().map_err(|_| RfsError::Unavailable)?;

        // 驗證標籤是否允許（ERR 一律允許）
        if tag != "ERR" && !allowed_tags.contains(&tag) {
            return Err(RfsError::Untrusted);
        }

        if cursor.len() < len {
            return Err(RfsError::Unavailable);
        }
        let data = cursor[..len].to_vec();
        cursor = &cursor[len..];

        // 必須緊跟一個換行（\n 或 \r\n）
        if cursor.starts_with(b"\n") {
            cursor = &cursor[1..];
        } else if cursor.starts_with(b"\r\n") {
            cursor = &cursor[2..];
        } else {
            return Err(RfsError::Unavailable);
        }

        frames.push(RfsFrame {
            tag: tag.to_string(),
            data,
        });
    }
}

// ───────────────────── 純函式：腳本產生器與解析器 ─────────────────────

pub fn script_common_header() -> String {
    let dir_helpers = am_base::outbox_remote::directory_identity_helpers();
    let file_helpers = am_base::outbox_remote::file_identity_helpers("/usr/bin/lsof");
    format!(
        r#"umask 077
if stat -c %Y . >/dev/null 2>&1; then G=1; else G=; fi
{dir_helpers}
{file_helpers}
"#
    )
}

pub fn resolve_folder_script(path: &str, instance: Option<&str>) -> String {
    let header = script_common_header();
    let root_rel = am_base::hosts::remote_root_for(instance);
    format!(
        r#"{header}
D={d}
am_enter_dir
case "$?" in
  0) ;;
  2) printf 'AM_RFS1\nERR 8\nNOTFOUND\nAM_RFS_DONE\n'; exit 0 ;;
  *) printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0 ;;
esac
P=$(pwd -P)
H=$(cd -P "$HOME" 2>/dev/null && pwd -P || printf '%s' "$HOME")
R=$(cd -P "$HOME/{root_rel}" 2>/dev/null && pwd -P || printf '%s' "$HOME/{root_rel}")
if [ -O . ]; then O=1; else O=0; fi
printf 'AM_RFS1\n'
printf 'PHYSICAL %d\n%s\n' "${{#P}}" "$P"
printf 'HOME %d\n%s\n' "${{#H}}" "$H"
printf 'ROOT %d\n%s\n' "${{#R}}" "$R"
printf 'OWNED %d\n%s\n' "${{#O}}" "$O"
printf 'AM_RFS_DONE\n'
"#,
        d = sh_quote(path),
        root_rel = root_rel,
    )
}

pub fn parse_resolve_folder(out: &[u8]) -> Result<ResolvedFolder, RfsError> {
    let frames = parse_rfs_frames(out, &["PHYSICAL", "HOME", "ROOT", "OWNED"])?;
    for f in &frames {
        if f.tag == "ERR" {
            let msg = String::from_utf8_lossy(&f.data);
            if msg.contains("NOTFOUND") {
                return Err(RfsError::NotFound);
            }
            return Err(RfsError::Untrusted);
        }
    }
    let mut physical = None;
    let mut home_physical = None;
    let mut root_physical = None;
    let mut owned_by_me = None;
    for f in frames {
        match f.tag.as_str() {
            "PHYSICAL" => physical = Some(String::from_utf8(f.data).map_err(|_| RfsError::Unavailable)?),
            "HOME" => home_physical = Some(String::from_utf8(f.data).map_err(|_| RfsError::Unavailable)?),
            "ROOT" => root_physical = Some(String::from_utf8(f.data).map_err(|_| RfsError::Unavailable)?),
            "OWNED" => {
                let s = String::from_utf8(f.data).map_err(|_| RfsError::Unavailable)?;
                owned_by_me = Some(s.trim() == "1");
            }
            _ => {}
        }
    }
    match (physical, home_physical, root_physical, owned_by_me) {
        (Some(p), Some(h), Some(r), Some(o)) => Ok(ResolvedFolder {
            physical: p,
            home_physical: h,
            root_physical: r,
            owned_by_me: o,
        }),
        _ => Err(RfsError::Unavailable),
    }
}

pub fn create_folder_script(root: &str, name: &str) -> String {
    let header = script_common_header();
    format!(
        r#"{header}
mkdir -m 700 -p {root_quoted} 2>/dev/null
D={root_quoted}
am_enter_dir || {{ printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}
N={name_quoted}
if [ -e "$N" ] || [ -L "$N" ]; then
  printf 'AM_RFS1\nERR 6\nEXISTS\nAM_RFS_DONE\n'; exit 0
fi
if ! mkdir -m 700 -- "$N" 2>/dev/null; then
  if [ -e "$N" ] || [ -L "$N" ]; then
    printf 'AM_RFS1\nERR 6\nEXISTS\nAM_RFS_DONE\n'; exit 0
  else
    printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0
  fi
fi
P=$(cd -P -- "$N" 2>/dev/null && pwd -P)
if [ -z "$P" ]; then
  printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0
fi
printf 'AM_RFS1\n'
printf 'PATH %d\n%s\n' "${{#P}}" "$P"
printf 'AM_RFS_DONE\n'
"#,
        root_quoted = sh_quote(root),
        name_quoted = sh_quote(name),
    )
}

pub fn parse_create_folder(out: &[u8]) -> Result<String, RfsError> {
    let frames = parse_rfs_frames(out, &["PATH"])?;
    for f in &frames {
        if f.tag == "ERR" {
            let msg = String::from_utf8_lossy(&f.data);
            if msg.contains("EXISTS") {
                return Err(RfsError::Exists);
            }
            return Err(RfsError::Untrusted);
        }
    }
    for f in frames {
        if f.tag == "PATH" {
            return String::from_utf8(f.data).map_err(|_| RfsError::Unavailable);
        }
    }
    Err(RfsError::Unavailable)
}

pub fn remove_created_folder_script(workspace: &str) -> String {
    let header = script_common_header();
    let basename = Path::new(workspace).file_name().and_then(|s| s.to_str()).unwrap_or("");
    format!(
        r#"{header}
D={ws_quoted}
am_enter_dir || {{ printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}
rmdir inbox 2>/dev/null || true
cd -P .. 2>/dev/null || true
rmdir {base_quoted} 2>/dev/null || true
printf 'AM_RFS1\nOK 0\n\nAM_RFS_DONE\n'
"#,
        ws_quoted = sh_quote(workspace),
        base_quoted = sh_quote(basename),
    )
}

pub fn ensure_inbox_script(workspace: &str) -> String {
    let header = script_common_header();
    format!(
        r#"{header}
D={ws_quoted}
am_enter_dir
case "$?" in
  0) ;;
  2) printf 'AM_RFS1\nERR 8\nNOTFOUND\nAM_RFS_DONE\n'; exit 0 ;;
  *) printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0 ;;
esac
if [ -e inbox ] || [ -L inbox ]; then
  if [ -L inbox ] || [ ! -d inbox ]; then
    printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0
  fi
  chmod 700 inbox 2>/dev/null || true
else
  mkdir -m 700 inbox 2>/dev/null || {{ printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}
fi
cd -P inbox 2>/dev/null || {{ printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}
if [ ! "$D/inbox" -ef . ]; then
  printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0
fi
printf 'AM_RFS1\nOK 0\n\nAM_RFS_DONE\n'
"#,
        ws_quoted = sh_quote(workspace),
    )
}

pub fn instructions_script(workspace: &str) -> String {
    let header = script_common_header();
    format!(
        r#"{header}
D={ws_quoted}
am_enter_dir || {{ printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}
printf 'AM_RFS1\n'

read_one() {{
  label=$1
  rel=$2
  F=$rel
  if [ ! -f "$F" ] || [ -L "$F" ]; then return 0; fi
  exec 3< "$F" || return 0
  if ! am_same; then exec 3<&-; return 0; fi
  if ! am_singlelink; then exec 3<&-; return 0; fi
  content=$(head -c 32768 <&3)
  exec 3<&-
  [ -n "$content" ] || return 0
  payload=$(printf '%s\t%s' "$label" "$content")
  len=$(printf '%s' "$payload" | wc -c | tr -d ' ')
  printf 'FILE %d\n%s\n' "$len" "$payload"
}}

for f in CLAUDE.md .claude/CLAUDE.md AGENTS.md; do
  [ -f "$f" ] && read_one "資料夾的指示:$f" "$f"
done

read_mem_dir() {{
  mdir=$1
  [ -d "$mdir" ] || return 0
  if [ -f "$mdir/MEMORY.md" ]; then
    read_one "記憶:$mdir/MEMORY.md" "$mdir/MEMORY.md"
  fi
  cnt=0
  for f in "$mdir"/*.md; do
    [ -f "$f" ] || continue
    bn=${{f##*/}}
    [ "$bn" = "MEMORY.md" ] && continue
    case "$bn" in .*|'*') continue;; esac
    cnt=$((cnt+1))
    if [ "$cnt" -gt 64 ]; then break; fi
    read_one "記憶:$mdir/$bn" "$f"
  done
}}

read_mem_dir "memory"
read_mem_dir ".claude/memory"

printf 'AM_RFS_DONE\n'
"#,
        ws_quoted = sh_quote(workspace),
    )
}

pub fn parse_instructions(out: &[u8]) -> Result<String, RfsError> {
    const TOTAL_MAX: usize = 96 * 1024;
    let frames = parse_rfs_frames(out, &["FILE"])?;
    for f in &frames {
        if f.tag == "ERR" {
            return Err(RfsError::Untrusted);
        }
    }
    let mut out_str = String::new();
    for f in frames {
        if f.tag != "FILE" {
            continue;
        }
        let line = String::from_utf8(f.data).map_err(|_| RfsError::Unavailable)?;
        let mut parts = line.splitn(2, '\t');
        let label = parts.next().unwrap_or("").trim();
        let content = parts.next().unwrap_or("").trim_end();
        if content.is_empty() {
            continue;
        }
        if out_str.len() + content.len() > TOTAL_MAX {
            out_str.push_str(&format!("\n（{label} 太大，沒有載入）\n"));
            continue;
        }
        out_str.push_str(&format!("\n\n## {label}\n\n{content}\n"));
    }
    Ok(out_str)
}

pub fn encode_private_files_stdin(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(format!("{RFS_VERSION_TAG}\n").as_bytes());
    for (name, content) in files {
        let name_bytes = name.as_bytes();
        let len = name_bytes.len() + 1 + content.len();
        buf.extend_from_slice(format!("FILE {len}\n").as_bytes());
        buf.extend_from_slice(name_bytes);
        buf.push(b'\t');
        buf.extend_from_slice(content);
        buf.push(b'\n');
    }
    buf.extend_from_slice(format!("{RFS_DONE_TAG}\n").as_bytes());
    buf
}

pub fn write_private_files_script(dir: &str) -> String {
    let header = script_common_header();
    format!(
        r#"{header}
mkdir -m 700 -p {dir_quoted} 2>/dev/null
D={dir_quoted}
am_enter_dir || {{ printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}

if command -v sha256sum >/dev/null 2>&1; then
  shacmd="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  shacmd="shasum -a 256"
else
  printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0
fi

# 解碼 stdin 的 AM_RFS1 框並寫入
IFS= read -r line
[ "$line" = "{ver}" ] || {{ printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}

printf 'AM_RFS1\n'
while IFS= read -r line; do
  [ "$line" = "{done_tag}" ] && break
  case "$line" in
    FILE\ *)
      len=${{line#FILE }}
      payload=$(head -c "$len")
      name=${{payload%%	*}}
      data=${{payload#*	}}
      tmp=$(mktemp ./tmp.XXXXXX 2>/dev/null) || {{ printf 'ERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}
      chmod 600 "$tmp"
      printf '%s' "$data" > "$tmp"
      mv -f "$tmp" "$name"
      chmod 600 "$name"
      hash=$($shacmd "$name" | awk '{{print $1}}')
      res=$(printf '%s\t%s' "$name" "$hash")
      rlen=$(printf '%s' "$res" | wc -c | tr -d ' ')
      printf 'HASH %d\n%s\n' "$rlen" "$res"
      # 跳過 payload 後的換行
      head -c 1 >/dev/null
      ;;
    *)
      printf 'ERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0
      ;;
  esac
done
printf 'AM_RFS_DONE\n'
"#,
        dir_quoted = sh_quote(dir),
        ver = RFS_VERSION_TAG,
        done_tag = RFS_DONE_TAG,
    )
}

pub fn parse_write_private_files(out: &[u8], files: &[(&str, &[u8])]) -> Result<(), RfsError> {
    use sha2::{Digest, Sha256};
    let frames = parse_rfs_frames(out, &["HASH"])?;
    for f in &frames {
        if f.tag == "ERR" {
            return Err(RfsError::Untrusted);
        }
    }
    let mut returned_hashes = HashMap::new();
    for f in frames {
        if f.tag == "HASH" {
            let line = String::from_utf8(f.data).map_err(|_| RfsError::Unavailable)?;
            let mut parts = line.splitn(2, '\t');
            if let (Some(name), Some(hash)) = (parts.next(), parts.next()) {
                returned_hashes.insert(name.to_string(), hash.trim().to_string());
            }
        }
    }

    for (name, content) in files {
        let expected_hash = format!("{:x}", Sha256::digest(content));
        let got_hash = returned_hashes.get(*name).ok_or(RfsError::Untrusted)?;
        if *got_hash != expected_hash {
            return Err(RfsError::Untrusted);
        }
    }
    Ok(())
}

pub fn mark_share_keep_script(outbox: &str, keep: bool) -> String {
    let header = script_common_header();
    let keep_flag = if keep { "1" } else { "0" };
    format!(
        r#"{header}
D={outbox_quoted}
if [ "{keep_flag}" = "1" ]; then
  mkdir -m 700 -p "$D" 2>/dev/null
  am_enter_dir || {{ printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}
  # outbox 是 bot 能寫的地方：標記檔若被換成符號連結，`touch`／`chmod` 會改到別的檔。先拆掉連結，再用 noclobber 建（O_EXCL，不寫穿）。
  [ -L .am-share-keep ] && rm -f .am-share-keep
  if [ ! -e .am-share-keep ]; then
    ( set -C; printf 'share bot outbox: share retention policy (SPEC 20, #850)\n' > .am-share-keep ) 2>/dev/null
  fi
else
  if am_enter_dir; then
    rm -f .am-share-keep
  fi
fi
printf 'AM_RFS1\nOK 0\n\nAM_RFS_DONE\n'
"#,
        outbox_quoted = sh_quote(outbox),
        keep_flag = keep_flag,
    )
}

pub fn inbox_write_script(
    workspace: &str,
    stored_name: &str,
    data_len: usize,
    max_bytes: u64,
    max_files: usize,
) -> String {
    let header = script_common_header();
    format!(
        r#"{header}
D={ws_quoted}/inbox
am_enter_dir || {{ printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}

total_size=0
file_count=0
for f in ./*; do
  [ -f "$f" ] && [ ! -L "$f" ] || continue
  file_count=$((file_count + 1))
  if [ -n "$G" ]; then
    sz=$(stat -L -c '%s' "$f" 2>/dev/null) || continue
  else
    sz=$(stat -L -f '%z' "$f" 2>/dev/null) || continue
  fi
  total_size=$((total_size + sz))
done

new_size=$((total_size + {data_len}))
new_count=$((file_count + 1))
if [ "$new_size" -gt {max_bytes} ] || [ "$new_count" -gt {max_files} ]; then
  printf 'AM_RFS1\nERR 4\nFULL\nAM_RFS_DONE\n'; exit 0
fi

N={name_quoted}
if [ -e "$N" ] || [ -L "$N" ]; then
  printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0
fi

set -C
: > "$N" 2>/dev/null || {{ printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}
set +C
chmod 600 "$N"

F="$N"
exec 3< "$F" || {{ rm -f "$N"; printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}
if ! am_same || ! am_singlelink; then
  exec 3<&-
  rm -f "$N"
  printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0
fi
exec 3<&-

head -c {data_len} > "$N"
actual_len=$(wc -c < "$N" | tr -d ' ')
if [ "$actual_len" -ne {data_len} ]; then
  rm -f "$N"
  printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0
fi

printf 'AM_RFS1\nOK 0\n\nAM_RFS_DONE\n'
"#,
        ws_quoted = sh_quote(workspace),
        name_quoted = sh_quote(stored_name),
        data_len = data_len,
        max_bytes = max_bytes,
        max_files = max_files,
    )
}

pub fn parse_inbox_write(out: &[u8]) -> Result<(), InboxError> {
    let frames = parse_rfs_frames(out, &["OK"]).map_err(|_| InboxError::Unavailable)?;
    for f in &frames {
        if f.tag == "ERR" {
            let msg = String::from_utf8_lossy(&f.data);
            if msg.contains("FULL") {
                return Err(InboxError::Full);
            }
            return Err(InboxError::Unavailable);
        }
    }
    if frames.iter().any(|f| f.tag == "OK") {
        Ok(())
    } else {
        Err(InboxError::Unavailable)
    }
}

pub fn inbox_has_script(workspace: &str, names: &[String]) -> String {
    let header = script_common_header();
    let mut checks = String::new();
    for (_i, name) in names.iter().enumerate() {
        let qn = sh_quote(name);
        checks.push_str(&format!(
            r#"F={qn}
if [ -f "$F" ] && [ ! -L "$F" ]; then
  exec 3< "$F" 2>/dev/null
  if [ $? -eq 0 ]; then
    if am_same && am_singlelink; then res="${{res}}1"; else res="${{res}}0"; fi
    exec 3<&-
  else
    res="${{res}}0"
  fi
else
  res="${{res}}0"
fi
"#
        ));
    }
    format!(
        r#"{header}
D={ws_quoted}/inbox
am_enter_dir || {{ printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}
res=""
{checks}
len=${{#res}}
printf 'AM_RFS1\n'
printf 'HAS %d\n%s\n' "$len" "$res"
printf 'AM_RFS_DONE\n'
"#,
        ws_quoted = sh_quote(workspace),
        checks = checks,
    )
}

pub fn parse_inbox_has(out: &[u8], count: usize) -> Result<Vec<bool>, RfsError> {
    let frames = parse_rfs_frames(out, &["HAS"])?;
    for f in &frames {
        if f.tag == "ERR" {
            return Err(RfsError::Untrusted);
        }
    }
    for f in frames {
        if f.tag == "HAS" {
            let s = String::from_utf8(f.data).map_err(|_| RfsError::Unavailable)?;
            let bits: Vec<bool> = s.trim().chars().map(|c| c == '1').collect();
            if bits.len() == count {
                return Ok(bits);
            }
        }
    }
    Err(RfsError::Unavailable)
}

pub fn outbox_list_script(outbox: &str) -> String {
    let header = script_common_header();
    format!(
        r#"{header}
D={outbox_quoted}
am_enter_dir
case "$?" in
  0) ;;
  2) printf 'AM_RFS1\nAM_RFS_DONE\n'; exit 0 ;;
  *) printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0 ;;
esac

printf 'AM_RFS1\n'
for f in ./*; do
  [ -f "$f" ] && [ ! -L "$f" ] || continue
  N=${{f##*/}}
  case "$N" in *[[:cntrl:]]*) continue;; esac
  F="$f"
  exec 3< "$F" || continue
  if ! am_same || ! am_singlelink; then exec 3<&-; continue; fi
  if [ -n "$G" ]; then
    m=$(stat -L -c '%s %Y %Z' /dev/fd/3 2>/dev/null) || {{ exec 3<&-; continue; }}
  else
    m=$(stat -L -f '%z %m %c' /dev/fd/3 2>/dev/null) || {{ exec 3<&-; continue; }}
  fi
  h=$(head -c 64 <&3 | od -An -tx1 | tr -d ' \n')
  exec 3<&-
  payload=$(printf '%s\t%s\t%s' "$m" "$h" "$N")
  len=$(printf '%s' "$payload" | wc -c | tr -d ' ')
  printf 'ENTRY %d\n%s\n' "$len" "$payload"
done
printf 'AM_RFS_DONE\n'
"#,
        outbox_quoted = sh_quote(outbox),
    )
}

fn hex_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len() / 2).filter_map(|i| u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()).collect()
}

pub fn parse_outbox_list(out: &[u8], _now: u64) -> Result<Vec<Value>, RfsError> {
    let frames = parse_rfs_frames(out, &["ENTRY"])?;
    for f in &frames {
        if f.tag == "ERR" {
            return Err(RfsError::Untrusted);
        }
    }
    let mut files = Vec::new();
    for f in frames {
        if f.tag != "ENTRY" {
            continue;
        }
        let line = String::from_utf8(f.data).map_err(|_| RfsError::Unavailable)?;
        let mut parts = line.splitn(3, '\t');
        let (Some(meta), Some(hex), Some(name)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let mut meta_parts = meta.split_whitespace();
        let (Some(Ok(size)), Some(Ok(modified))) = (meta_parts.next().map(str::parse::<u64>), meta_parts.next().map(str::parse::<u64>)) else { continue };
        let changed = meta_parts.next().and_then(|c| c.parse::<u64>().ok()).unwrap_or(0);
        if name.is_empty() || name.contains('/') || name.chars().any(char::is_control) {
            continue;
        }
        if withheld_name(name) {
            continue;
        }
        let head = hex_bytes(hex);
        if content_is_withheld(&head) {
            continue;
        }
        files.push((name.to_string(), size, modified, changed));
    }

    files.sort_by(|a, b| b.2.max(b.3).cmp(&a.2.max(a.3)).then_with(|| a.0.cmp(&b.0)));
    files.truncate(am_base::outbox::MAX_ENTRIES);

    let entries: Vec<Value> = files
        .into_iter()
        .map(|(name, size, modified, changed)| {
            json!({
                "name": name,
                "size": size,
                "modified": modified,
                "changed": changed,
                "kept": true,
                "keep_days": 14,
                "ttl_secs": Value::Null,
            })
        })
        .collect();

    Ok(entries)
}

pub fn outbox_stream_script(outbox: &str, name: &str, max: u64) -> String {
    let header = script_common_header();
    format!(
        r#"{header}
D={outbox_quoted}
am_enter_dir
case "$?" in
  0) ;;
  2) printf 'AM_RFS1\nERR 8\nNOTFOUND\nAM_RFS_DONE\n'; exit 0 ;;
  *) printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0 ;;
esac
F={name_quoted}
if [ ! -f "$F" ] || [ -L "$F" ]; then
  printf 'AM_RFS1\nERR 8\nNOTFOUND\nAM_RFS_DONE\n'; exit 0
fi
exec 3< "$F" || {{ printf 'AM_RFS1\nERR 8\nNOTFOUND\nAM_RFS_DONE\n'; exit 0; }}
if ! am_same || ! am_singlelink; then
  exec 3<&-
  printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0
fi
if [ -n "$G" ]; then
  sz=$(stat -L -c '%s' /dev/fd/3 2>/dev/null) || {{ exec 3<&-; printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}
else
  sz=$(stat -L -f '%z' /dev/fd/3 2>/dev/null) || {{ exec 3<&-; printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}
fi
if [ "$sz" -gt {max} ]; then
  exec 3<&-
  printf 'AM_RFS1\nERR 8\nTOOLARGE\nAM_RFS_DONE\n'; exit 0
fi
printf 'AM_RFS1\nFILE %d\n' "$sz"
head -c "$sz" <&3
exec 3<&-
"#,
        outbox_quoted = sh_quote(outbox),
        name_quoted = sh_quote(name),
        max = max,
    )
}

pub fn outbox_read_script(outbox: &str, name: &str, max: u64) -> String {
    let header = script_common_header();
    format!(
        r#"{header}
D={outbox_quoted}
am_enter_dir
case "$?" in
  0) ;;
  2) printf 'AM_RFS1\nERR 8\nNOTFOUND\nAM_RFS_DONE\n'; exit 0 ;;
  *) printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0 ;;
esac
F={name_quoted}
if [ ! -f "$F" ] || [ -L "$F" ]; then
  printf 'AM_RFS1\nERR 8\nNOTFOUND\nAM_RFS_DONE\n'; exit 0
fi
exec 3< "$F" || {{ printf 'AM_RFS1\nERR 8\nNOTFOUND\nAM_RFS_DONE\n'; exit 0; }}
if ! am_same || ! am_singlelink; then
  exec 3<&-
  printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0
fi
if [ -n "$G" ]; then
  st=$(stat -L -c '%i %s %Y000000000 %Z000000000' /dev/fd/3 2>/dev/null) || {{ exec 3<&-; printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}
  sz=$(stat -L -c '%s' /dev/fd/3 2>/dev/null) || {{ exec 3<&-; printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}
else
  st=$(stat -L -f '%i %z %m000000000 %c000000000' /dev/fd/3 2>/dev/null) || {{ exec 3<&-; printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}
  sz=$(stat -L -f '%z' /dev/fd/3 2>/dev/null) || {{ exec 3<&-; printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}
fi
if [ "$sz" -gt {max} ]; then
  exec 3<&-
  printf 'AM_RFS1\nERR 8\nTOOLARGE\nAM_RFS_DONE\n'; exit 0
fi
st_len=$(printf '%s' "$st" | wc -c | tr -d ' ')
printf 'AM_RFS1\n'
printf 'STAT %d\n%s\n' "$st_len" "$st"
printf 'DATA %d\n' "$sz"
head -c "$sz" <&3
printf '\n'
exec 3<&-
printf 'AM_RFS_DONE\n'
"#,
        outbox_quoted = sh_quote(outbox),
        name_quoted = sh_quote(name),
        max = max,
    )
}

pub fn parse_outbox_read(out: &[u8], name: &str) -> Result<ReadWithStat, ShareFileError> {
    let frames = parse_rfs_frames(out, &["STAT", "DATA"]).map_err(|_| ShareFileError::Unavailable)?;
    for f in &frames {
        if f.tag == "ERR" {
            let msg = String::from_utf8_lossy(&f.data);
            if msg.contains("NOTFOUND") {
                return Err(ShareFileError::NotFound);
            }
            if msg.contains("TOOLARGE") {
                return Err(ShareFileError::TooLarge);
            }
            return Err(ShareFileError::Unavailable);
        }
    }
    let mut stat_info = None;
    let mut data = None;
    for f in frames {
        if f.tag == "STAT" {
            let s = String::from_utf8(f.data).map_err(|_| ShareFileError::Unavailable)?;
            let mut it = s.split_whitespace();
            let (Some(Ok(ino)), Some(Ok(size)), Some(Ok(mtime_ns)), Some(Ok(ctime_ns))) = (
                it.next().map(str::parse::<u64>),
                it.next().map(str::parse::<u64>),
                it.next().map(str::parse::<i128>),
                it.next().map(str::parse::<i128>),
            ) else {
                return Err(ShareFileError::Unavailable);
            };
            stat_info = Some((ino, size, mtime_ns, ctime_ns));
        } else if f.tag == "DATA" {
            data = Some(f.data);
        }
    }
    match (stat_info, data) {
        (Some((ino, _size, mtime_ns, ctime_ns)), Some(data)) => Ok(ReadWithStat {
            name: name.to_string(),
            data,
            ino,
            mtime_ns,
            ctime_ns,
        }),
        _ => Err(ShareFileError::Unavailable),
    }
}

pub fn photo_stats_script(workspace: &str, rels: &[Vec<String>]) -> String {
    let header = script_common_header();
    let mut body = String::new();
    for (idx, rel) in rels.iter().enumerate() {
        let mut enter_chain = String::new();
        let leaf = rel.last().map(|s| s.as_str()).unwrap_or("");
        for part in &rel[..rel.len().saturating_sub(1)] {
            enter_chain.push_str(&format!(
                r#"[ -L {p} ] && bad=1; cd -P {p} 2>/dev/null || bad=1; "#,
                p = sh_quote(part)
            ));
        }
        body.push_str(&format!(
            r#"(
  bad=0
  D={ws_quoted}
  am_enter_dir || bad=1
  {enter_chain}
  F={leaf_quoted}
  if [ "$bad" -eq 0 ] && [ -f "$F" ] && [ ! -L "$F" ]; then
    exec 3< "$F" || bad=1
    if [ "$bad" -eq 0 ] && am_same && am_singlelink; then
      if [ -n "$G" ]; then
        st=$(stat -L -c '%i %s %Y000000000' /dev/fd/3 2>/dev/null) || bad=1
      else
        st=$(stat -L -f '%i %z %m000000000' /dev/fd/3 2>/dev/null) || bad=1
      fi
      exec 3<&-
      if [ "$bad" -eq 0 ]; then
        payload=$(printf '%d\t%s' "{idx}" "$st")
        len=$(printf '%s' "$payload" | wc -c | tr -d ' ')
        printf 'STAT %d\n%s\n' "$len" "$payload"
      else
        payload=$(printf '%d\tNONE' "{idx}")
        len=$(printf '%s' "$payload" | wc -c | tr -d ' ')
        printf 'STAT %d\n%s\n' "$len" "$payload"
      fi
    else
      exec 3<&-
      payload=$(printf '%d\tNONE' "{idx}")
      len=$(printf '%s' "$payload" | wc -c | tr -d ' ')
      printf 'STAT %d\n%s\n' "$len" "$payload"
    fi
  else
    payload=$(printf '%d\tNONE' "{idx}")
    len=$(printf '%s' "$payload" | wc -c | tr -d ' ')
    printf 'STAT %d\n%s\n' "$len" "$payload"
  fi
)
"#,
            ws_quoted = sh_quote(workspace),
            enter_chain = enter_chain,
            leaf_quoted = sh_quote(leaf),
            idx = idx,
        ));
    }
    format!(
        r#"{header}
printf 'AM_RFS1\n'
{body}
printf 'AM_RFS_DONE\n'
"#,
        header = header,
        body = body,
    )
}

pub fn parse_photo_stats(out: &[u8], count: usize) -> Result<Vec<Option<PhotoStat>>, RfsError> {
    let frames = parse_rfs_frames(out, &["STAT"])?;
    for f in &frames {
        if f.tag == "ERR" {
            return Err(RfsError::Untrusted);
        }
    }
    let mut stats = vec![None; count];
    for f in frames {
        if f.tag != "STAT" {
            continue;
        }
        let line = String::from_utf8(f.data).map_err(|_| RfsError::Unavailable)?;
        let mut it = line.split_whitespace();
        let Some(Ok(idx)) = it.next().map(str::parse::<usize>) else { continue };
        if idx >= count {
            continue;
        }
        let (Some(Ok(ino)), Some(Ok(size)), Some(Ok(mtime_ns))) = (
            it.next().map(str::parse::<u64>),
            it.next().map(str::parse::<u64>),
            it.next().map(str::parse::<i128>),
        ) else {
            continue;
        };
        stats[idx] = Some(PhotoStat { ino, size, mtime_ns });
    }
    Ok(stats)
}

pub fn photo_fetch_script(
    workspace: &str,
    rels: &[Vec<String>],
    each_max: u64,
    total_max: u64,
) -> String {
    let header = script_common_header();
    let mut body = String::new();
    for (idx, rel) in rels.iter().enumerate() {
        let mut enter_chain = String::new();
        let leaf = rel.last().map(|s| s.as_str()).unwrap_or("");
        for part in &rel[..rel.len().saturating_sub(1)] {
            enter_chain.push_str(&format!(
                r#"[ -L {p} ] && bad=1; cd -P {p} 2>/dev/null || bad=1; "#,
                p = sh_quote(part)
            ));
        }
        body.push_str(&format!(
            r#"(
  bad=0
  D={ws_quoted}
  am_enter_dir || bad=1
  {enter_chain}
  F={leaf_quoted}
  if [ "$bad" -eq 0 ] && [ -f "$F" ] && [ ! -L "$F" ]; then
    exec 3< "$F" || bad=1
    if [ "$bad" -eq 0 ] && am_same && am_singlelink; then
      if [ -n "$G" ]; then
        sz=$(stat -L -c '%s' /dev/fd/3 2>/dev/null) || bad=1
      else
        sz=$(stat -L -f '%z' /dev/fd/3 2>/dev/null) || bad=1
      fi
      # 合計在送出之前判斷：已送出的大小累計在 $T（子 shell 改不了父 shell 變數）（#987）。
      used=$(awk '{{s+=$1}} END{{print s+0}}' "$T")
      if [ "$bad" -eq 0 ] && [ "$sz" -le {each_max} ] && [ $(( used + sz )) -le {total_max} ]; then
        prefix_len=$(printf '%d\tOK\n' "{idx}" | wc -c | tr -d ' ')
        total_len=$(( prefix_len + sz ))
        printf 'PHOTO %d\n%d\tOK\n' "$total_len" "{idx}"
        head -c "$sz" <&3
        printf '\n'
        echo "$sz" >> "$T"
      else
        exec 3<&-
        prefix=$(printf '%d\tERR\tsource_too_large' "{idx}")
        plen=$(printf '%s' "$prefix" | wc -c | tr -d ' ')
        printf 'PHOTO %d\n%s\n' "$plen" "$prefix"
      fi
      exec 3<&-
    else
      exec 3<&-
      prefix=$(printf '%d\tERR\tnot_found' "{idx}")
      plen=$(printf '%s' "$prefix" | wc -c | tr -d ' ')
      printf 'PHOTO %d\n%s\n' "$plen" "$prefix"
    fi
  else
    prefix=$(printf '%d\tERR\tnot_found' "{idx}")
    plen=$(printf '%s' "$prefix" | wc -c | tr -d ' ')
    printf 'PHOTO %d\n%s\n' "$plen" "$prefix"
  fi
)
"#,
            ws_quoted = sh_quote(workspace),
            enter_chain = enter_chain,
            leaf_quoted = sh_quote(leaf),
            idx = idx,
            each_max = each_max,
            total_max = total_max,
        ));
    }
    format!(
        r#"{header}
T=$(mktemp "${{TMPDIR:-/tmp}}/am-rfs-photo.XXXXXX") || {{ printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}
trap 'rm -f "$T"' EXIT
printf 'AM_RFS1\n'
{body}
printf 'AM_RFS_DONE\n'
"#,
        header = header,
        body = body,
    )
}

pub fn parse_photo_fetch(
    out: &[u8],
    rels: &[Vec<String>],
    _each_max: u64,
    total_max: u64,
) -> Result<Vec<Result<Vec<u8>, &'static str>>, RfsError> {
    let count = rels.len();
    let frames = parse_rfs_frames(out, &["PHOTO"])?;
    for f in &frames {
        if f.tag == "ERR" {
            return Err(RfsError::Untrusted);
        }
    }
    let mut results = vec![Err("not_found"); count];
    let mut total_accum = 0u64;

    for f in frames {
        if f.tag != "PHOTO" {
            continue;
        }
        let data = &f.data;
        let line_end = data.iter().position(|&b| b == b'\n').unwrap_or(data.len());
        let meta_str = std::str::from_utf8(&data[..line_end]).map_err(|_| RfsError::Unavailable)?;
        let mut it = meta_str.splitn(3, '\t');
        let Some(Ok(idx)) = it.next().map(str::parse::<usize>) else { continue };
        if idx >= count {
            continue;
        }
        let status = it.next().unwrap_or("ERR");
        if status == "OK" {
            let body_bytes = if line_end < data.len() { &data[line_end + 1..] } else { &[] };
            let body_len = body_bytes.len() as u64;
            if total_accum + body_len > total_max {
                results[idx] = Err("source_too_large");
            } else {
                total_accum += body_len;
                results[idx] = Ok(body_bytes.to_vec());
            }
        } else {
            let reason = it.next().unwrap_or("not_found");
            if reason == "source_too_large" {
                results[idx] = Err("source_too_large");
            } else {
                results[idx] = Err("not_found");
            }
        }
    }
    Ok(results)
}

pub fn measure_script(workspace: &str, outbox: &str) -> String {
    let header = script_common_header();
    format!(
        r#"{header}
fail() {{ printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}
# 一棵樹：`du -skx`（不跟符號連結、不跨檔案系統；KiB）＋`find -xdev -type f` 數檔案（上限 200001 個就停）。任何一步量不出數字就整個失敗，不當成 0。
ws_kib=0
ws_files=0
D={ws_quoted}
am_enter_dir
case "$?" in
  0)
    ws_kib=$(du -skx . 2>/dev/null | awk '{{print $1}}')
    ws_files=$(find . -xdev -type f 2>/dev/null | head -n 200001 | wc -l | tr -d ' ')
    ;;
  2) ;; # 工作目錄不見了：算 0（跟本機一樣）
  *) fail ;;
esac
ob_kib=0
ob_files=0
D={ob_quoted}
am_enter_dir
case "$?" in
  0)
    ob_kib=$(du -skx . 2>/dev/null | awk '{{print $1}}')
    ob_files=$(find . -xdev -type f 2>/dev/null | head -n 200001 | wc -l | tr -d ' ')
    # 標記檔是 daemon 放的，不算使用者的量（連同它佔的區塊一起扣）。
    if [ -f .am-share-keep ] && [ ! -L .am-share-keep ]; then
      kk=$(du -sk .am-share-keep 2>/dev/null | awk '{{print $1}}')
      case "$kk" in ''|*[!0-9]*) fail ;; esac
      ob_kib=$(( ob_kib - kk ))
      [ "$ob_files" -gt 0 ] && ob_files=$(( ob_files - 1 ))
    fi
    ;;
  2) ;;
  *) fail ;;
esac
for v in "$ws_kib" "$ws_files" "$ob_kib" "$ob_files"; do
  case "$v" in ''|*[!0-9]*) fail ;; esac
done
trunc=0
if [ "$ws_files" -gt 200000 ] || [ "$ob_files" -gt 200000 ]; then trunc=1; fi
tot_files=$(( ws_files + ob_files ))
payload="$(( ws_kib * 1024 )) $(( ob_kib * 1024 )) $tot_files $trunc"
printf 'AM_RFS1\n'
printf 'MEASURE %d\n%s\n' "${{#payload}}" "$payload"
printf 'AM_RFS_DONE\n'
"#,
        ws_quoted = sh_quote(workspace),
        ob_quoted = sh_quote(outbox),
    )
}

pub fn parse_measure(out: &[u8]) -> Result<budget::Measured, RfsError> {
    let frames = parse_rfs_frames(out, &["MEASURE"])?;
    for f in &frames {
        if f.tag == "ERR" {
            return Err(RfsError::Untrusted);
        }
    }
    for f in frames {
        if f.tag == "MEASURE" {
            let s = String::from_utf8(f.data).map_err(|_| RfsError::Unavailable)?;
            let mut it = s.split_whitespace();
            let (Some(Ok(ws)), Some(Ok(ob)), Some(Ok(files)), Some(trunc_str)) = (
                it.next().map(str::parse::<u64>),
                it.next().map(str::parse::<u64>),
                it.next().map(str::parse::<u64>),
                it.next(),
            ) else {
                return Err(RfsError::Unavailable);
            };
            return Ok(budget::Measured {
                workspace_bytes: ws,
                outbox_bytes: ob,
                files,
                truncated: trunc_str == "1",
            });
        }
    }
    Err(RfsError::Unavailable)
}

pub fn prune_outbox_script(outbox: &str, keep_days: u64, cap_bytes: u64, cap_files: usize) -> String {
    prune_outbox_script_at(outbox, keep_days, cap_bytes, cap_files, None)
}

/// 同 [`prune_outbox_script`]；`now` 是測試用的時鐘接縫（epoch 秒，同 `outbox-gc.sh` 的 `OUTBOX_GC_NOW`）：
/// ctime 沒辦法往回改，只能把「現在」往後撥來驗「超過 14 天」。正式一律 `None`＝遠端的 `date +%s`。
pub fn prune_outbox_script_at(outbox: &str, keep_days: u64, cap_bytes: u64, cap_files: usize, now: Option<u64>) -> String {
    let header = script_common_header();
    let now_expr = now.map_or_else(|| "$(date +%s)".to_string(), |n| n.to_string());
    format!(
        r#"{header}
D={ob_quoted}
am_enter_dir
case "$?" in
  0) ;;
  2) printf 'AM_RFS1\nPRUNED 1\n0\nAM_RFS_DONE\n'; exit 0 ;;
  *) printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0 ;;
esac
# 分享保留政策（#850，跟 `outbox-gc.sh` 的 `share_prune` 同語意）：遠端從不遞迴刪目錄，只刪一般檔；符號連結不跟、不算。
# 輸出的刪除數靠 `echo x` 計（檔名含換行也數得對）。
removed=0
NL=$(printf '\n.'); NL=${{NL%.}}
rm_each='for f; do rm -f -- "$f" && echo x; done'
fail() {{ printf 'AM_RFS1\nERR 9\nUNTRUSTED\nAM_RFS_DONE\n'; exit 0; }}
# 1. 檔名含換行的直接刪（清單與下載都不收這種名字）。
n=$(find . -type f -name "*${{NL}}*" -exec sh -c "$rm_each" sh {{}} + 2>/dev/null | wc -l | tr -d ' ')
removed=$(( removed + ${{n:-0}} ))
# 2. mtime 與 ctime 都超過保留天數的刪（標記檔不動）：用參考檔比，GNU／BSD 的 find 都有 -newer 與 -newercm。
now={now_expr}
cutoff=$(( now - {keep_days} * 86400 ))
tmp=$(mktemp "${{TMPDIR:-/tmp}}/am-rfs-prune.XXXXXX") || fail
over="$tmp.over"
trap 'rm -f "$tmp" "$over"' EXIT
# GNU `date -d @N` 先試：BSD 的 `date -r N` 在 GNU 上是「讀檔案 N 的 mtime」，而 cwd 就是 bot 能寫的 outbox（檔名可以叫 `1760000000`）。
touch -t "$(date -d "@$cutoff" +%Y%m%d%H%M.%S 2>/dev/null || date -r "$cutoff" +%Y%m%d%H%M.%S)" "$tmp" || fail
n=$(find . -type f ! -name .am-share-keep ! -newer "$tmp" ! -newercm "$tmp" -exec sh -c "$rm_each" sh {{}} + 2>/dev/null | wc -l | tr -d ' ')
removed=$(( removed + ${{n:-0}} ))
# 3. 超量：新的排前面累計，數量或位元組一超過上限，那一個和更舊的都刪。
if [ -n "$G" ]; then
  find . -type f ! -name .am-share-keep -exec stat -c '%Y %s %n' {{}} + 2>/dev/null > "$tmp"
else
  find . -type f ! -name .am-share-keep -exec stat -f '%m %z %N' {{}} + 2>/dev/null > "$tmp"
fi
sort -rn -k1,1 "$tmp" | awk -v cb={cap_bytes} -v cf={cap_files} '{{ n++; t += $2; if (n > cf || t > cb) {{ sub(/^[0-9]+ [0-9]+ /, ""); print }} }}' > "$over"
while IFS= read -r f; do
  [ -n "$f" ] || continue
  rm -f -- "$f" && removed=$(( removed + 1 ))
done < "$over"
printf 'AM_RFS1\n'
printf 'PRUNED %d\n%s\n' "${{#removed}}" "$removed"
printf 'AM_RFS_DONE\n'
"#,
        ob_quoted = sh_quote(outbox),
        keep_days = keep_days,
        now_expr = now_expr,
        cap_bytes = cap_bytes,
        cap_files = cap_files,
    )
}

pub fn parse_prune_outbox(out: &[u8]) -> Result<u64, RfsError> {
    let frames = parse_rfs_frames(out, &["PRUNED"])?;
    for f in &frames {
        if f.tag == "ERR" {
            return Err(RfsError::Untrusted);
        }
    }
    for f in frames {
        if f.tag == "PRUNED" {
            let s = String::from_utf8(f.data).map_err(|_| RfsError::Unavailable)?;
            return s.trim().parse::<u64>().map_err(|_| RfsError::Unavailable);
        }
    }
    Err(RfsError::Unavailable)
}

// ───────────────────── RemoteSite 實作 ─────────────────────

/// 建受限分享 bot 前、以及每次啟動前的遠端檢查（R-S2 §4.1）。順序就是檢查的順序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightError {
    /// 主機沒連線、或 ssh 失敗：不確定就當連不上（fail closed）。
    Unreachable,
    /// `claude --version` 讀不到或低於最低版本；`found` 是讀到的版本（讀不到是空字串）。
    ClaudeTooOld { found: String },
    /// 遠端有 claude 的 managed settings：它優先於 `--settings`，籠子保證不了。
    ManagedSettings,
}

/// `claude --version` 的輸出（例如 `2.1.288 (Claude Code)`）裡第一個 `x.y[.z…]`。
pub fn parse_claude_version(out: &str) -> Option<String> {
    out.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_ascii_digit() && c != '.'))
        .find(|w| {
            let parts: Vec<&str> = w.split('.').collect();
            parts.len() >= 2 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        })
        .map(str::to_string)
}

/// 版本逐段比較（`2.1.288` ≥ `2.1.287`，`2.10` ≥ `2.9`）。解析不了就是 `false`。
pub fn version_at_least(found: &str, min: &str) -> bool {
    let parse = |s: &str| -> Option<Vec<u64>> { s.split('.').map(|p| p.parse().ok()).collect() };
    let (Some(f), Some(m)) = (parse(found), parse(min)) else { return false };
    let n = f.len().max(m.len());
    (0..n)
        .map(|i| (f.get(i).copied().unwrap_or(0), m.get(i).copied().unwrap_or(0)))
        .find(|(a, b)| a != b)
        .is_none_or(|(a, b)| a > b)
}

/// managed settings 的位置（Linux 與 macOS 各兩處：單檔與 `.d` 目錄）。有檔、或目錄裡有東西就回 `FOUND`。
pub fn managed_settings_script() -> String {
    r#"umask 077
for f in /etc/claude-code/managed-settings.json "/Library/Application Support/ClaudeCode/managed-settings.json"; do
  if [ -f "$f" ]; then echo FOUND; exit 0; fi
done
for d in /etc/claude-code/managed-settings.d "/Library/Application Support/ClaudeCode/managed-settings.d"; do
  if [ -d "$d" ] && [ -n "$(ls -A "$d" 2>/dev/null)" ]; then echo FOUND; exit 0; fi
done
echo NONE
"#
    .to_string()
}

/// 遠端只要主機連得上（信任分享用）。
pub fn preflight_connected(conn: &HostConn) -> Result<(), PreflightError> {
    if conn.is_connected() {
        Ok(())
    } else {
        Err(PreflightError::Unreachable)
    }
}

/// 受限分享用的完整檢查：連線、claude 版本（≥ `min_claude`）、沒有 managed settings。
/// 版本那一趟吞掉「找不到 claude」（只回 `NOCLAUDE`）：ssh 本身失敗才是 `Unreachable`。
pub async fn preflight_restricted(conn: &HostConn, min_claude: &str) -> Result<(), PreflightError> {
    preflight_connected(conn)?;
    let _permit = acquire_slot(&conn.name, TIMEOUT_QUICK).await.map_err(|_| PreflightError::Unreachable)?;
    let ver = conn
        .ssh_exec_path_timeout("claude --version 2>/dev/null || printf NOCLAUDE", TIMEOUT_QUICK)
        .await
        .map_err(|e| {
            tracing::warn!(host = %conn.name, error = %e, "preflight: claude --version ssh failed");
            PreflightError::Unreachable
        })?;
    let found = parse_claude_version(&ver).unwrap_or_default();
    if found.is_empty() || !version_at_least(&found, min_claude) {
        return Err(PreflightError::ClaudeTooOld { found });
    }
    let managed = conn.ssh_exec_timeout(&managed_settings_script(), TIMEOUT_QUICK).await.map_err(|e| {
        tracing::warn!(host = %conn.name, error = %e, "preflight: managed settings check ssh failed");
        PreflightError::Unreachable
    })?;
    if managed.contains("FOUND") {
        return Err(PreflightError::ManagedSettings);
    }
    Ok(())
}

impl RemoteSite {
    // 建立／啟動（R-S2 用）
    pub async fn resolve_folder(conn: &HostConn, path: &str) -> Result<ResolvedFolder, RfsError> {
        if !conn.is_connected() {
            return Err(RfsError::Unavailable);
        }
        let _permit = acquire_slot(&conn.name, TIMEOUT_QUICK).await?;
        let script = resolve_folder_script(path, conn.instance());
        let out = conn.ssh_exec_timeout(&script, TIMEOUT_QUICK).await.map_err(|e| {
            tracing::warn!(host = %conn.name, error = %e, "resolve_folder ssh failed");
            RfsError::Unavailable
        })?;
        parse_resolve_folder(out.as_bytes())
    }

    pub async fn create_folder(conn: &HostConn, root: &str, name: &str) -> Result<String, RfsError> {
        if !conn.is_connected() {
            return Err(RfsError::Unavailable);
        }
        let _permit = acquire_slot(&conn.name, TIMEOUT_QUICK).await?;
        let script = create_folder_script(root, name);
        let out = conn.ssh_exec_timeout(&script, TIMEOUT_QUICK).await.map_err(|e| {
            tracing::warn!(host = %conn.name, error = %e, "create_folder ssh failed");
            RfsError::Unavailable
        })?;
        parse_create_folder(out.as_bytes())
    }

    pub async fn remove_created_folder(&self) -> Result<(), RfsError> {
        if !self.conn.is_connected() {
            return Err(RfsError::Unavailable);
        }
        let _permit = acquire_slot(&self.host, TIMEOUT_QUICK).await?;
        let script = remove_created_folder_script(&self.workspace);
        let out = self.conn.ssh_exec_timeout(&script, TIMEOUT_QUICK).await.map_err(|e| {
            tracing::warn!(host = %self.host, error = %e, "remove_created_folder ssh failed");
            RfsError::Unavailable
        })?;
        let frames = parse_rfs_frames(out.as_bytes(), &["OK"])?;
        if frames.iter().any(|f| f.tag == "OK") {
            Ok(())
        } else {
            Err(RfsError::Unavailable)
        }
    }

    pub async fn ensure_inbox(&self) -> Result<(), RfsError> {
        if !self.conn.is_connected() {
            return Err(RfsError::Unavailable);
        }
        let _permit = acquire_slot(&self.host, TIMEOUT_QUICK).await?;
        let script = ensure_inbox_script(&self.workspace);
        let out = self.conn.ssh_exec_timeout(&script, TIMEOUT_QUICK).await.map_err(|e| {
            tracing::warn!(host = %self.host, error = %e, "ensure_inbox ssh failed");
            RfsError::Unavailable
        })?;
        let frames = parse_rfs_frames(out.as_bytes(), &["OK"])?;
        for f in &frames {
            if f.tag == "ERR" {
                let msg = String::from_utf8_lossy(&f.data);
                if msg.contains("NOTFOUND") {
                    return Err(RfsError::NotFound);
                }
                return Err(RfsError::Untrusted);
            }
        }
        if frames.iter().any(|f| f.tag == "OK") {
            Ok(())
        } else {
            Err(RfsError::Unavailable)
        }
    }

    pub async fn instructions(&self) -> Result<String, RfsError> {
        if !self.conn.is_connected() {
            return Err(RfsError::Unavailable);
        }
        let _permit = acquire_slot(&self.host, TIMEOUT_QUICK).await?;
        let script = instructions_script(&self.workspace);
        let out = self.conn.ssh_exec_timeout(&script, TIMEOUT_QUICK).await.map_err(|e| {
            tracing::warn!(host = %self.host, error = %e, "instructions ssh failed");
            RfsError::Unavailable
        })?;
        parse_instructions(out.as_bytes())
    }

    pub async fn write_private_files(
        conn: &HostConn,
        dir: &str,
        files: &[(&str, &[u8])],
    ) -> Result<(), RfsError> {
        if !conn.is_connected() {
            return Err(RfsError::Unavailable);
        }
        let _permit = acquire_slot(&conn.name, TIMEOUT_QUICK).await?;
        let script = write_private_files_script(dir);
        let stdin = encode_private_files_stdin(files);
        let out = conn.ssh_exec_stdin(&script, &stdin, TIMEOUT_QUICK).await.map_err(|e| {
            tracing::warn!(host = %conn.name, error = %e, "write_private_files ssh failed");
            RfsError::Unavailable
        })?;
        parse_write_private_files(out.as_bytes(), files)
    }

    pub async fn mark_share_keep(&self, keep: bool) -> Result<(), RfsError> {
        if !self.conn.is_connected() {
            return Err(RfsError::Unavailable);
        }
        let _permit = acquire_slot(&self.host, TIMEOUT_QUICK).await?;
        let script = mark_share_keep_script(&self.outbox, keep);
        let out = self.conn.ssh_exec_timeout(&script, TIMEOUT_QUICK).await.map_err(|e| {
            tracing::warn!(host = %self.host, error = %e, "mark_share_keep ssh failed");
            RfsError::Unavailable
        })?;
        let frames = parse_rfs_frames(out.as_bytes(), &["OK"])?;
        if frames.iter().any(|f| f.tag == "OK") {
            Ok(())
        } else {
            Err(RfsError::Unavailable)
        }
    }

    // 入口（R-S3 用）
    pub async fn inbox_write(
        &self,
        stored_name: &str,
        data: &[u8],
        max_bytes: u64,
        max_files: usize,
    ) -> Result<(), InboxError> {
        if !self.conn.is_connected() {
            return Err(InboxError::Unavailable);
        }
        let _permit = acquire_slot(&self.host, TIMEOUT_UPLOAD).await.map_err(|_| InboxError::Unavailable)?;
        let script = inbox_write_script(&self.workspace, stored_name, data.len(), max_bytes, max_files);
        let out = self.conn.ssh_exec_stdin(&script, data, TIMEOUT_UPLOAD).await.map_err(|e| {
            tracing::warn!(host = %self.host, error = %e, "inbox_write ssh failed");
            InboxError::Unavailable
        })?;
        parse_inbox_write(out.as_bytes())
    }

    pub async fn inbox_has(&self, names: &[String]) -> Result<Vec<bool>, RfsError> {
        if !self.conn.is_connected() {
            return Err(RfsError::Unavailable);
        }
        let _permit = acquire_slot(&self.host, TIMEOUT_QUICK).await?;
        let script = inbox_has_script(&self.workspace, names);
        let out = self.conn.ssh_exec_timeout(&script, TIMEOUT_QUICK).await.map_err(|e| {
            tracing::warn!(host = %self.host, error = %e, "inbox_has ssh failed");
            RfsError::Unavailable
        })?;
        parse_inbox_has(out.as_bytes(), names.len())
    }

    pub async fn outbox_list(&self, now: u64) -> Result<Vec<Value>, RfsError> {
        if !self.conn.is_connected() {
            return Err(RfsError::Unavailable);
        }
        let _permit = acquire_slot(&self.host, TIMEOUT_QUICK).await?;
        let script = outbox_list_script(&self.outbox);
        let out = self.conn.ssh_exec_timeout(&script, TIMEOUT_QUICK).await.map_err(|e| {
            tracing::warn!(host = %self.host, error = %e, "outbox_list ssh failed");
            RfsError::Unavailable
        })?;
        parse_outbox_list(out.as_bytes(), now)
    }

    pub async fn outbox_stream(&self, name: &str, max: u64) -> Result<RemoteFile, ShareFileError> {
        use tokio::io::AsyncReadExt;
        if !self.conn.is_connected() {
            return Err(ShareFileError::Unavailable);
        }
        // 下載名額不排隊：拿不到直接 429 (ShareFileError::Unavailable)
        let sem = host_slot(&self.host);
        let permit = sem.try_acquire_owned().map_err(|_| ShareFileError::Unavailable)?;

        if name.is_empty() || name.contains('/') || name.chars().any(char::is_control) || withheld_name(name) {
            return Err(ShareFileError::NotFound);
        }

        let script = outbox_stream_script(&self.outbox, name, max);
        let mut stream = self.conn.ssh_stream(&script, &[]).await.map_err(|_| ShareFileError::Unavailable)?;

        // 讀取標頭首行（超時 30 秒）
        async fn read_line(stream: &mut SshStream) -> Vec<u8> {
            let mut line_buf = Vec::new();
            let mut b = [0u8; 1];
            while stream.read_exact(&mut b).await.is_ok() {
                line_buf.push(b[0]);
                if b[0] == b'\n' {
                    break;
                }
            }
            line_buf
        }
        let read_header_fut = async {
            let mut line_buf = read_line(&mut stream).await;
            if line_buf.starts_with(RFS_VERSION_TAG.as_bytes()) {
                line_buf = read_line(&mut stream).await;
            }
            // 錯誤框是 `ERR <長度>\n<原因>\n`：原因在下一行（NOTFOUND／TOOLARGE／UNTRUSTED），要讀進來才分得出 404／413／503。
            if line_buf.starts_with(b"ERR") {
                let reason = read_line(&mut stream).await;
                line_buf.extend_from_slice(&reason);
            }
            line_buf
        };

        let line = tokio::time::timeout(TIMEOUT_STREAM_HEAD, read_header_fut)
            .await
            .map_err(|_| ShareFileError::Unavailable)?;

        let line_str = std::str::from_utf8(&line).map_err(|_| ShareFileError::Unavailable)?.trim();
        if line_str.starts_with("ERR") {
            if line_str.contains("NOTFOUND") {
                return Err(ShareFileError::NotFound);
            }
            if line_str.contains("TOOLARGE") {
                return Err(ShareFileError::TooLarge);
            }
            return Err(ShareFileError::Unavailable);
        }

        if !line_str.starts_with("FILE") {
            return Err(ShareFileError::Unavailable);
        }

        let len: u64 = line_str
            .strip_prefix("FILE")
            .and_then(|s| s.trim().parse().ok())
            .ok_or(ShareFileError::Unavailable)?;

        if len > max {
            return Err(ShareFileError::TooLarge);
        }

        // 讀取前 64 bytes 檢查內容是否被保留
        let head_to_read = len.min(64) as usize;
        let mut head = vec![0u8; head_to_read];
        tokio::time::timeout(TIMEOUT_STREAM_HEAD, stream.read_exact(&mut head))
            .await
            .map_err(|_| ShareFileError::Unavailable)?
            .map_err(|_| ShareFileError::Unavailable)?;

        if content_is_withheld(&head) {
            return Err(ShareFileError::NotFound);
        }

        Ok(RemoteFile {
            len,
            head,
            body: stream,
            permit,
        })
    }

    pub async fn outbox_read(&self, name: &str, max: u64) -> Result<ReadWithStat, ShareFileError> {
        if !self.conn.is_connected() {
            return Err(ShareFileError::Unavailable);
        }
        let _permit = acquire_slot(&self.host, TIMEOUT_QUICK).await.map_err(|_| ShareFileError::Unavailable)?;
        if name.is_empty() || name.contains('/') || name.chars().any(char::is_control) || withheld_name(name) {
            return Err(ShareFileError::NotFound);
        }
        let script = outbox_read_script(&self.outbox, name, max);
        let out = self.conn.ssh_exec_timeout(&script, TIMEOUT_QUICK).await.map_err(|e| {
            tracing::warn!(host = %self.host, error = %e, "outbox_read ssh failed");
            ShareFileError::Unavailable
        })?;
        parse_outbox_read(out.as_bytes(), name)
    }

    pub async fn photo_stats(&self, rels: &[Vec<String>]) -> Result<Vec<Option<PhotoStat>>, RfsError> {
        if !self.conn.is_connected() {
            return Err(RfsError::Unavailable);
        }
        let _permit = acquire_slot(&self.host, TIMEOUT_QUICK).await?;
        let script = photo_stats_script(&self.workspace, rels);
        let out = self.conn.ssh_exec_timeout(&script, TIMEOUT_QUICK).await.map_err(|e| {
            tracing::warn!(host = %self.host, error = %e, "photo_stats ssh failed");
            RfsError::Unavailable
        })?;
        parse_photo_stats(out.as_bytes(), rels.len())
    }

    pub async fn photo_fetch(
        &self,
        rels: &[Vec<String>],
        each_max: u64,
        total_max: u64,
    ) -> Result<Vec<Result<Vec<u8>, &'static str>>, RfsError> {
        if !self.conn.is_connected() {
            return Err(RfsError::Unavailable);
        }
        let _permit = acquire_slot(&self.host, TIMEOUT_MAINTAIN).await?;
        let script = photo_fetch_script(&self.workspace, rels, each_max, total_max);
        // 照片是二進位，走原始位元組（見 `exec_bytes`）。讀取上限＝合計上限＋每筆框頭的餘裕；遠端超過就當不可信（#987）。
        let cap = total_max.saturating_add(64 * 1024 * rels.len() as u64).saturating_add(4096);
        use tokio::io::AsyncReadExt as _;
        let mut stream = self.conn.ssh_stream(&script, &[]).await.map_err(|e| {
            tracing::warn!(host = %self.host, error = %e, "photo_fetch ssh failed");
            RfsError::Unavailable
        })?;
        let mut out = Vec::new();
        match tokio::time::timeout(TIMEOUT_MAINTAIN, stream.take(cap.saturating_add(1)).read_to_end(&mut out)).await {
            Ok(Ok(_)) if (out.len() as u64) <= cap => {}
            _ => return Err(RfsError::Unavailable),
        }
        parse_photo_fetch(&out, rels, each_max, total_max)
    }

    // 預算與保留（R-S4 用）
    pub async fn measure(&self) -> Result<budget::Measured, RfsError> {
        if !self.conn.is_connected() {
            return Err(RfsError::Unavailable);
        }
        let _permit = acquire_slot(&self.host, TIMEOUT_MAINTAIN).await?;
        let script = measure_script(&self.workspace, &self.outbox);
        let out = self.conn.ssh_exec_timeout(&script, TIMEOUT_MAINTAIN).await.map_err(|e| {
            tracing::warn!(host = %self.host, error = %e, "measure ssh failed");
            RfsError::Unavailable
        })?;
        parse_measure(out.as_bytes())
    }

    pub async fn prune_outbox(
        &self,
        keep_days: u64,
        cap_bytes: u64,
        cap_files: usize,
    ) -> Result<u64, RfsError> {
        if !self.conn.is_connected() {
            return Err(RfsError::Unavailable);
        }
        let _permit = acquire_slot(&self.host, TIMEOUT_MAINTAIN).await?;
        let script = prune_outbox_script(&self.outbox, keep_days, cap_bytes, cap_files);
        let out = self.conn.ssh_exec_timeout(&script, TIMEOUT_MAINTAIN).await.map_err(|e| {
            tracing::warn!(host = %self.host, error = %e, "prune_outbox ssh failed");
            RfsError::Unavailable
        })?;
        parse_prune_outbox(out.as_bytes())
    }
}

// ───────────────────── 測試替身支援 ─────────────────────

#[cfg(test)]
pub mod test_support {
    use super::*;
    use std::process::Stdio;

    pub fn local_sh_fake(host: &str, scratch: &Path) {
        let scratch_buf = scratch.to_path_buf();
        let scratch_clone = scratch_buf.clone();

        am_base::hosts::set_ssh_fake_io(host, move |script: &str, stdin: &[u8]| {
            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.arg("-c").arg(script);
            cmd.env("HOME", &scratch_clone);
            cmd.env("TMPDIR", &scratch_clone);
            cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("spawn /bin/sh: {e}"))?;
            if let Some(mut sin) = child.stdin.take() {
                use std::io::Write;
                sin.write_all(stdin).map_err(|e| anyhow::anyhow!("write stdin: {e}"))?;
            }
            let out = child.wait_with_output().map_err(|e| anyhow::anyhow!("wait /bin/sh: {e}"))?;
            Ok(out.stdout)
        });

        am_base::hosts::set_ssh_fake(host, move |script: &str| {
            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.arg("-c").arg(script);
            cmd.env("HOME", &scratch_buf);
            cmd.env("TMPDIR", &scratch_buf);
            cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
            let out = cmd.output().map_err(|e| anyhow::anyhow!("exec /bin/sh: {e}"))?;
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        });
    }
}
