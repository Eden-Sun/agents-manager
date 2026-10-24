//! Descriptor-relative traversal: canonicalize for policy, then O_NOFOLLOW every component.
//! No shell expansion. Even a symlink swapped after canonicalization cannot escape the root fd.
use serde_json::Value;
use std::{
    ffi::CString,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::fd::{AsRawFd, FromRawFd},
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};
const MAX_FILE: usize = 1024 * 1024;
const MAX_OUTPUT: usize = 16 * 1024;
pub struct Editor {
    root: PathBuf,
    directory: File,
}
impl Editor {
    pub fn new(root: &Path) -> Result<Self, String> {
        let root = root.canonicalize().map_err(|_| "invalid worktree")?;
        let directory = File::open(&root).map_err(|_| "worktree cannot be opened")?;
        if !directory
            .metadata()
            .map_err(|_| "worktree metadata failed")?
            .is_dir()
        {
            return Err("worktree is not a directory".into());
        }
        Ok(Self { root, directory })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    fn open(&self, path: &str, create: bool, write: bool) -> Result<File, String> {
        let given = Path::new(path);
        // Both the actual host worktree and the stable Bash mount name are accepted.
        let relative = if given.is_absolute() {
            given
                .strip_prefix(&self.root)
                .or_else(|_| given.strip_prefix("/workspace"))
                .map_err(|_| "path outside worktree")?
        } else {
            given
        };
        let parts: Vec<_> = relative
            .components()
            .filter(|part| !matches!(part, Component::CurDir))
            .map(|part| match part {
                Component::Normal(s) => s.to_str().ok_or("invalid path"),
                _ => Err("invalid path traversal"),
            })
            .collect::<Result<_, _>>()?;
        if parts.is_empty() {
            if !write {
                return self
                    .directory
                    .try_clone()
                    .map_err(|_| "worktree fd unavailable".into());
            }
            return Err("file path required".into());
        }
        let candidate = parts.iter().fold(self.root.clone(), |p, part| p.join(part));
        let checked = if create {
            candidate.parent().ok_or("invalid parent")?.canonicalize()
        } else {
            candidate.canonicalize()
        }
        .map_err(|_| "path cannot be resolved")?;
        if !checked.starts_with(&self.root) {
            return Err("symlink escape denied".into());
        }
        let mut directory = self
            .directory
            .try_clone()
            .map_err(|_| "worktree fd unavailable")?;
        for (index, part) in parts.iter().enumerate() {
            let last = index + 1 == parts.len();
            let name = CString::new(*part).map_err(|_| "invalid path")?;
            let flags = libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | if !last {
                    libc::O_RDONLY | libc::O_DIRECTORY
                } else if create {
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
                } else if write {
                    libc::O_RDWR
                } else {
                    libc::O_RDONLY
                };
            let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0o600) };
            if fd < 0 {
                return Err("path open refused".into());
            }
            directory = unsafe { File::from_raw_fd(fd) };
        }
        let metadata = directory
            .metadata()
            .map_err(|_| "file metadata unavailable")?;
        if (!metadata.is_file() && (write || !metadata.is_dir()))
            || (write && metadata.nlink() != 1)
        {
            return Err("only single-link regular files are editable".into());
        }
        if metadata.len() > MAX_FILE as u64 {
            return Err("file size limit exceeded".into());
        }
        Ok(directory)
    }
    pub fn execute(&self, input: &Value) -> Result<String, String> {
        let command = input
            .get("command")
            .and_then(Value::as_str)
            .ok_or("missing editor command")?;
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .ok_or("missing path")?;
        let field = |key| {
            input
                .get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("missing {key}"))
        };
        if !matches!(command, "view" | "create" | "str_replace" | "insert") {
            return Err("unsupported editor command".into());
        }
        // Validate new content BEFORE O_CREAT: invalid/oversize input must not leave an empty file.
        let created = if command == "create" {
            let content = field("file_text")?;
            if content.len() > MAX_FILE {
                return Err("file size limit exceeded".into());
            }
            Some(content)
        } else {
            None
        };
        let mut file = self.open(path, created.is_some(), command != "view")?;
        if command == "view"
            && file
                .metadata()
                .map_err(|_| "metadata unavailable")?
                .is_dir()
        {
            if input.get("view_range").is_some() {
                return Err("view_range requires a file".into());
            }
            return list_directory(&file);
        }
        if let Some(content) = created {
            file.write_all(content.as_bytes())
                .and_then(|_| file.sync_all())
                .map_err(|_| "file write failed")?;
            return Ok("created".into());
        }
        let mut text = String::new();
        (&mut file)
            .take((MAX_FILE + 1) as u64)
            .read_to_string(&mut text)
            .map_err(|_| "file read failed or not UTF-8")?;
        if text.len() > MAX_FILE {
            return Err("file size limit exceeded".into());
        }
        if command == "view" {
            let lines: Vec<_> = text.lines().collect();
            let (start, end) = if let Some(range) = input.get("view_range") {
                let range = range
                    .as_array()
                    .filter(|a| a.len() == 2)
                    .ok_or("invalid view_range")?;
                let start = range[0]
                    .as_i64()
                    .filter(|n| *n >= 1)
                    .ok_or("invalid view_range")?;
                let end = range[1].as_i64().ok_or("invalid view_range")?;
                let end = if end == -1 { lines.len() as i64 } else { end };
                if end < start || start > lines.len() as i64 || end > lines.len() as i64 {
                    return Err("invalid view_range".into());
                }
                (start as usize - 1, end as usize)
            } else {
                (0, lines.len())
            };
            let mut result = String::new();
            for (i, line) in lines[start..end].iter().enumerate() {
                use std::fmt::Write;
                let _ = writeln!(result, "{}\t{}", start + i + 1, line);
                if result.len() > MAX_OUTPUT {
                    return Err("view output limit exceeded; use view_range".into());
                }
            }
            return Ok(result);
        }
        let updated = if command == "str_replace" {
            let old = field("old_str")?;
            let new = field("new_str")?;
            if old.is_empty() || text.matches(old).count() != 1 {
                return Err("old_str must match exactly once".into());
            }
            text.replacen(old, new, 1)
        } else {
            let line = input
                .get("insert_line")
                .and_then(Value::as_u64)
                .ok_or("invalid insert_line")? as usize;
            let new = field("new_str")?;
            let offset = if line == 0 {
                0
            } else {
                let mut lines = text.split_inclusive('\n');
                let mut offset = 0;
                for _ in 0..line {
                    offset += lines.next().ok_or("invalid insert_line")?.len();
                }
                offset
            };
            let mut result = text.clone();
            let prefix = if offset > 0 && !text[..offset].ends_with('\n') {
                "\n"
            } else {
                ""
            };
            result.insert_str(offset, &format!("{prefix}{new}\n"));
            result
        };
        if updated.len() > MAX_FILE {
            return Err("file size limit exceeded".into());
        }
        file.seek(SeekFrom::Start(0))
            .and_then(|_| file.write_all(updated.as_bytes()))
            .and_then(|_| file.set_len(updated.len() as u64))
            .and_then(|_| file.sync_all())
            .map_err(|_| "file write failed")?;
        Ok("updated".into())
    }
}

fn list_directory(file: &File) -> Result<String, String> {
    // Open a fresh directory description: dup() would share the readdir offset with
    // root/other views and could make the second view empty or race a concurrent view.
    let fd = unsafe {
        libc::openat(
            file.as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err("directory fd unavailable".into());
    }
    let raw = unsafe { libc::fdopendir(fd) };
    if raw.is_null() {
        unsafe {
            libc::close(fd);
        }
        return Err("directory could not be read".into());
    }
    struct Entries(*mut libc::DIR);
    impl Drop for Entries {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let entries = Entries(raw);
    let mut names = Vec::new();
    let mut bytes = 0;
    loop {
        let entry = unsafe { libc::readdir(entries.0) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        if name == "." || name == ".." {
            continue;
        }
        bytes += name.len() + 1;
        if bytes > MAX_OUTPUT {
            return Err("directory output limit exceeded".into());
        }
        names.push(name);
    }
    names.sort();
    Ok(names.join("\n"))
}
