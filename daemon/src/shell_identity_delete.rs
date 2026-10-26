//! User initiated removal of one directly declared shell-discovered Claude `ccN` alias.
//!
//! This only edits the one startup file whose strict candidate matches the live login-shell
//! alias. It never edits config.toml or Claude credentials/profile/history. See SPEC §16.4.

use crate::api::RequestPrincipal;
use crate::lc_error::{LcError, LcResult};
use crate::state::App;
use crate::{db, hosts};
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const HOST_IO_TIMEOUT: Duration = Duration::from_secs(20);
const PROBE_SENTINEL: &str = "__AGM_CC_ALIAS_PROBE_OK__";

// Deletions on one host serialize through the full reference-check/edit/probe/rollback/cache
// sequence. This deliberately is not a global identity-writer admission gate.
static HOST_LOCKS: OnceLock<Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();

fn host_lock(host: &str) -> Arc<tokio::sync::Mutex<()>> {
    HOST_LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(host.to_string())
        .or_default()
        .clone()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct IdentityRef {
    #[serde(rename = "type")]
    ref_type: &'static str,
    id: String,
}

impl IdentityRef {
    fn bot(id: String) -> Self {
        Self { ref_type: "bot", id }
    }

    fn runtime(id: String) -> Self {
        Self { ref_type: "runtime", id }
    }

    fn supervisor(id: String) -> Self {
        Self { ref_type: "supervisor", id }
    }

    fn role(id: String) -> Self {
        Self { ref_type: "supervisor_role", id }
    }
}

async fn first_reference(app: &App, host: &str, identity: &str) -> anyhow::Result<Option<IdentityRef>> {
    // A configured Bot reference blocks even when it is idle: it may be started later. A bare
    // Bot (identity=None) does not explicitly use cc0 merely because both use the default login.
    for bot in db::live_bots_on_host(&app.db, host).await? {
        if explicitly_references(&bot.kind, bot.identity.as_deref(), identity) {
            return Ok(Some(IdentityRef::bot(bot.id)));
        }
    }

    // Runtime identity catches a live pane whose effective identity differs from its Bot config.
    let runtime_run: Option<String> = sqlx::query_scalar(
        "SELECT r.id FROM runs r
         JOIN bots b ON b.id=r.bot_id
         JOIN projects p ON p.id=b.project_id
         WHERE p.host=? AND p.deleted_at IS NULL AND b.deleted_at IS NULL AND b.kind='claude'
           AND r.state IN ('starting','running','stopping') AND r.runtime_identity=?
         ORDER BY r.started_at DESC LIMIT 1",
    )
    .bind(host)
    .bind(identity)
    .fetch_optional(&app.db)
    .await?;
    if let Some(run_id) = runtime_run {
        return Ok(Some(IdentityRef::runtime(run_id)));
    }

    // The patrol identity is stored on `supervisors`; both supervisor tables associate it with a
    // host through project_id (or, for older rows without it, their linked Bot's project).
    // Unconfigured rows without either association are not a host reference.
    let supervisor: Option<String> = sqlx::query_scalar(
        "SELECT s.id FROM supervisors s
         LEFT JOIN projects sp ON sp.id=s.project_id AND sp.deleted_at IS NULL
         LEFT JOIN bots sb ON sb.id=s.bot_id AND sb.deleted_at IS NULL
         LEFT JOIN projects bp ON bp.id=sb.project_id AND bp.deleted_at IS NULL
         WHERE s.identity=? AND ((NULLIF(trim(s.project_id),'') IS NOT NULL AND sp.host=?)
                              OR (NULLIF(trim(s.project_id),'') IS NULL AND bp.host=?))
         ORDER BY s.id LIMIT 1",
    )
    .bind(identity)
    .bind(host)
    .bind(host)
    .fetch_optional(&app.db)
    .await?;
    if let Some(id) = supervisor {
        return Ok(Some(IdentityRef::supervisor(id)));
    }

    let role: Option<String> = sqlx::query_scalar(
        "SELECT sr.role FROM supervisor_roles sr
         LEFT JOIN projects sp ON sp.id=sr.project_id AND sp.deleted_at IS NULL
         LEFT JOIN bots sb ON sb.id=sr.bot_id AND sb.deleted_at IS NULL
         LEFT JOIN projects bp ON bp.id=sb.project_id AND bp.deleted_at IS NULL
         WHERE sr.identity=? AND ((NULLIF(trim(sr.project_id),'') IS NOT NULL AND sp.host=?)
                               OR (NULLIF(trim(sr.project_id),'') IS NULL AND bp.host=?))
         ORDER BY sr.role LIMIT 1",
    )
    .bind(identity)
    .bind(host)
    .bind(host)
    .fetch_optional(&app.db)
    .await?;
    if let Some(role) = role {
        return Ok(Some(IdentityRef::role(role)));
    }
    Ok(None)
}

fn conflict(
    host: &str,
    identity: &str,
    reason: &'static str,
    message: &'static str,
    backup_path: Option<&str>,
    refs: Option<Vec<IdentityRef>>,
    restored: Option<bool>,
) -> LcError {
    let mut extra = json!({"host":host,"kind":"claude","identity":identity,"message":message});
    if let Some(path) = backup_path {
        extra["backup_path"] = json!(path);
    }
    if let Some(refs) = refs {
        extra["refs"] = json!(refs);
    }
    if let Some(restored) = restored {
        extra["restored"] = json!(restored);
    }
    LcError::conflict(reason, extra)
}

fn invalid_identity(host: &str, identity: &str, message: &'static str) -> LcError {
    LcError::BadValue(json!({
        "error":"bad_request", "reason":"invalid_shell_identity", "message":message,
        "host":host, "kind":"claude", "identity":identity
    }))
}

#[derive(Debug, Deserialize)]
struct WorkerFile {
    text: Option<String>,
    symlink: bool,
    regular: bool,
    mode: u32,
    uid: u32,
    nlink: u64,
}

#[derive(Debug, Deserialize)]
struct Snapshot {
    home: String,
    shell: String,
    home_uid: u32,
    files: BTreeMap<String, Option<WorkerFile>>,
}

#[derive(Debug, Deserialize)]
struct WorkerReply {
    ok: bool,
    reason: Option<String>,
    home: Option<String>,
    shell: Option<String>,
    home_uid: Option<u32>,
    files: Option<BTreeMap<String, Option<WorkerFile>>>,
    backup_path: Option<String>,
    candidate_sha256: Option<String>,
    #[serde(default)]
    replaced: bool,
}

impl WorkerReply {
    fn snapshot(self, host: &str, identity: &str) -> Result<Snapshot, LcError> {
        if !self.ok {
            return Err(worker_error(host, identity, self.reason.as_deref().unwrap_or("source_unknown"), self.backup_path.as_deref()));
        }
        Ok(Snapshot {
            home: self.home.ok_or_else(|| conflict(host, identity, "alias_source_unknown", "The shell startup source could not be verified.", None, None, None))?,
            shell: self.shell.ok_or_else(|| conflict(host, identity, "alias_source_unknown", "The shell startup source could not be verified.", None, None, None))?,
            home_uid: self.home_uid.ok_or_else(|| conflict(host, identity, "alias_source_unknown", "The shell startup source could not be verified.", None, None, None))?,
            files: self.files.ok_or_else(|| conflict(host, identity, "alias_source_unknown", "The shell startup source could not be verified.", None, None, None))?,
        })
    }
}

#[derive(Serialize)]
struct WorkerRequest<'a> {
    action: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidate: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backup_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidate_sha256: Option<&'a str>,
    expected_home: &'a str,
}

// Sent over stdin to both the local Python process and HostConn::ssh_exec_stdin. It reads only
// candidate startup files, never prints their contents to logs/responses, and retains every backup.
const HOST_WORKER: &str = r#"
import hashlib,json,os,re,secrets,stat,subprocess,sys,tempfile

BASH_LOGIN_FILES=('bash_profile','bash_login','profile')
ZSH_FILES=('zshenv','zprofile','zshrc','zlogin')
class Failure(Exception):
    def __init__(self,reason,backup_path=None,replaced=False): self.reason=reason; self.backup_path=backup_path; self.replaced=replaced
def fail(reason,backup_path=None,replaced=False): raise Failure(reason,backup_path,replaced)
def out(**v): sys.stdout.write(json.dumps(v,separators=(',',':'))+'\n')
def sha(data): return hashlib.sha256(data).hexdigest()
def fsync_dir(path):
    try: fd=os.open(path,os.O_RDONLY)
    except OSError: return
    try:
        try: os.fsync(fd)
        except OSError: pass
    finally:
        try: os.close(fd)
        except OSError: pass
def metadata_safe(path):
    try:
        if os.listxattr(path,follow_symlinks=False): return False
    except OSError: return False
    if sys.platform=='darwin':
        try:
            result=subprocess.run(['/bin/ls','-lde',path],stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,text=True,timeout=3,check=False)
        except Exception: return False
        lines=result.stdout.splitlines()
        if result.returncode!=0 or not lines or '+' in lines[0].split()[0]: return False
    return True
def safe_home():
    h=os.environ.get('HOME') or os.path.expanduser('~')
    absolute=os.path.abspath(h)
    if not h or os.path.realpath(absolute)!=absolute: fail('source_unknown')
    st=os.stat(absolute,follow_symlinks=False)
    if not stat.S_ISDIR(st.st_mode) or st.st_uid!=os.geteuid() or st.st_mode & 0o022 or not metadata_safe(absolute): fail('source_unknown')
    return absolute,st.st_uid
def check_shell_location(home):
    shell=os.environ.get('SHELL','')
    if os.path.basename(shell)=='zsh':
        if 'ZDOTDIR' in os.environ:
            zdot=os.environ.get('ZDOTDIR','')
            zdot_abs=os.path.abspath(zdot) if zdot else ''
            if not zdot_abs or zdot_abs!=home or os.path.realpath(zdot_abs)!=home: fail('unsupported_zdotdir')
def read_startup(home,name,read_text=True):
    p=os.path.join(home,'.'+name)
    try: st=os.lstat(p)
    except FileNotFoundError: return None
    if stat.S_ISLNK(st.st_mode): return {'text':None,'symlink':True,'regular':False,'mode':stat.S_IMODE(st.st_mode),'uid':st.st_uid,'nlink':st.st_nlink}
    if not stat.S_ISREG(st.st_mode) or st.st_size>1048576:
        return {'text':None,'symlink':False,'regular':False,'mode':stat.S_IMODE(st.st_mode),'uid':st.st_uid,'nlink':st.st_nlink}
    if st.st_uid!=os.geteuid() or st.st_nlink!=1 or st.st_mode & (0o022|0o7000):
        return {'text':None,'symlink':False,'regular':True,'mode':stat.S_IMODE(st.st_mode),'uid':st.st_uid,'nlink':st.st_nlink}
    if not metadata_safe(p):
        return {'text':None,'symlink':False,'regular':True,'mode':stat.S_IMODE(st.st_mode),'uid':st.st_uid,'nlink':st.st_nlink}
    if not read_text:
        return {'text':None,'symlink':False,'regular':True,'mode':stat.S_IMODE(st.st_mode),'uid':st.st_uid,'nlink':st.st_nlink}
    fd=os.open(p,os.O_RDONLY|getattr(os,'O_NOFOLLOW',0))
    try:
        now=os.fstat(fd)
        if (now.st_dev,now.st_ino)!=(st.st_dev,st.st_ino): fail('unsafe_file')
        data=b''
        while True:
            chunk=os.read(fd,65536)
            if not chunk: break
            data+=chunk
            if len(data)>1048576: fail('source_unsupported')
    finally: os.close(fd)
    try: text=data.decode('utf-8')
    except UnicodeDecodeError: fail('source_unsupported')
    return {'text':text,'symlink':False,'regular':True,'mode':stat.S_IMODE(st.st_mode),'uid':st.st_uid,'nlink':st.st_nlink}
def target_path(home,profile):
    if profile not in ('bashrc','zshrc'): fail('source_unknown')
    return os.path.join(home,'.'+profile)
def read_target(home,profile):
    p=target_path(home,profile)
    try: st=os.lstat(p)
    except FileNotFoundError: fail('source_unknown')
    if stat.S_ISLNK(st.st_mode) or not stat.S_ISREG(st.st_mode) or st.st_nlink!=1 or st.st_uid!=os.geteuid() or st.st_mode & (0o022|0o7000):
        fail('unsafe_file')
    if not metadata_safe(p): fail('unsafe_file')
    fd=os.open(p,os.O_RDONLY|getattr(os,'O_NOFOLLOW',0))
    try:
        opened=os.fstat(fd)
        if (opened.st_dev,opened.st_ino)!=(st.st_dev,st.st_ino): fail('unsafe_file')
        data=b''
        while True:
            chunk=os.read(fd,65536)
            if not chunk: break
            data+=chunk
            if len(data)>1048576: fail('source_unsupported')
    finally: os.close(fd)
    return p,st,data
def backup_dir(home):
    current=home
    for part in ('.config','agents-manager','shell-alias-backups'):
        current=os.path.join(current,part)
        try: os.mkdir(current,0o700)
        except FileExistsError: pass
        st=os.lstat(current)
        if stat.S_ISLNK(st.st_mode) or not stat.S_ISDIR(st.st_mode) or st.st_uid!=os.geteuid() or st.st_mode & 0o022:
            fail('backup_failed')
        if not metadata_safe(current): fail('backup_failed')
        if part=='shell-alias-backups' and stat.S_IMODE(st.st_mode)!=0o700:
            fail('backup_failed')
    return current
def make_backup(home,profile,data,requested_path=None):
    directory=backup_dir(home)
    backup=requested_path or os.path.join(directory,os.path.basename(profile)+'-'+secrets.token_hex(16)+'.bak')
    if os.path.dirname(backup)!=directory or not re.fullmatch(re.escape(profile)+r'-[0-9a-f]{32}\.bak',os.path.basename(backup)): fail('backup_failed')
    fd=os.open(backup,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,'O_NOFOLLOW',0),0o600)
    created=os.fstat(fd)
    try:
        os.fchmod(fd,0o600)
        view=memoryview(data)
        while view:
            n=os.write(fd,view)
            if n<=0: fail('backup_failed',backup)
            view=view[n:]
        os.fsync(fd)
    except Exception:
        try: os.close(fd)
        except OSError: pass
        try:
            now=os.lstat(backup)
            if (now.st_dev,now.st_ino)==(created.st_dev,created.st_ino): os.unlink(backup)
        except OSError: pass
        fail('backup_failed')
    else: os.close(fd)
    try:
        now=os.lstat(backup)
        if (now.st_dev,now.st_ino)!=(created.st_dev,created.st_ino) or not stat.S_ISREG(now.st_mode) or now.st_nlink!=1 or now.st_uid!=os.geteuid() or stat.S_IMODE(now.st_mode)!=0o600:
            fail('backup_failed')
        if not metadata_safe(backup) or sha(read_backup(home,profile,backup))!=sha(data): fail('backup_failed')
    except Exception:
        try:
            now=os.lstat(backup)
            if (now.st_dev,now.st_ino)==(created.st_dev,created.st_ino): os.unlink(backup)
        except OSError: pass
        fail('backup_failed')
    try:
        dfd=os.open(directory,os.O_RDONLY)
        try: os.fsync(dfd)
        finally: os.close(dfd)
    except Exception: fail('backup_failed',backup)
    return backup
def write_temp(p,st,data):
    fd,tmp=tempfile.mkstemp(prefix='.agm-shell-alias-',dir=os.path.dirname(p))
    try:
        os.fchown(fd,st.st_uid,st.st_gid)
        os.fchmod(fd,stat.S_IMODE(st.st_mode))
        view=memoryview(data)
        while view:
            n=os.write(fd,view)
            if n<=0: fail('commit_failed')
            view=view[n:]
        os.fsync(fd)
    except Exception:
        os.close(fd)
        try: os.unlink(tmp)
        except OSError: pass
        raise
    os.close(fd)
    return tmp
def prepare_backup(home,profile,expected_sha,candidate,requested_path=None):
    p,st,old=read_target(home,profile)
    if sha(old)!=expected_sha: fail('file_changed')
    backup=make_backup(home,profile,old,requested_path)
    # Catch an edit made while the durable backup was being written. The backup is retained and
    # only returned once it has been reopened and verified; the startup file is still untouched.
    _,now_st,now=read_target(home,profile)
    if sha(now)!=expected_sha or (now_st.st_dev,now_st.st_ino)!=(st.st_dev,st.st_ino): fail('file_changed',backup)
    return backup,sha(candidate)
def syntax_check(shell,path,backup):
    if os.path.basename(shell) not in ('bash','zsh') or not os.path.isabs(shell): fail('unsupported_shell',backup)
    try:
        result=subprocess.run([shell,'-n',path],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=8,check=False)
    except Exception: fail('syntax_check_failed',backup)
    if result.returncode!=0: fail('syntax_check_failed',backup)
def commit_if_unchanged(home,profile,expected_sha,data,backup_path,candidate_sha):
    old_backup=read_backup(home,profile,backup_path)
    if sha(old_backup)!=expected_sha: fail('backup_mismatch',backup_path)
    if sha(data)!=candidate_sha: fail('candidate_changed',backup_path)
    p,st,old=read_target(home,profile)
    if sha(old)!=expected_sha or old!=old_backup: fail('file_changed',backup_path)
    tmp=None
    try:
        tmp=write_temp(p,st,data)
        syntax_check(os.environ.get('SHELL',''),tmp,backup_path)
        # Best-effort recheck of bytes and inode. The check-to-rename interval is not a kernel CAS.
        _,current_st,current=read_target(home,profile)
        if sha(current)!=expected_sha or (current_st.st_dev,current_st.st_ino)!=(st.st_dev,st.st_ino):
            fail('file_changed',backup_path)
        os.replace(tmp,p); tmp=None
        fsync_dir(os.path.dirname(p))
        return sha(data)
    except Failure as e:
        if tmp:
            try: os.unlink(tmp)
            except OSError: pass
        if e.backup_path is None: e.backup_path=backup_path
        if e.reason=='commit_failed' and os.path.exists(p):
            try:
                with open(p,'rb') as f: e.replaced=sha(f.read())==candidate_sha
            except Exception: pass
        raise
    except Exception:
        if tmp:
            try: os.unlink(tmp)
            except OSError: pass
        # `os.replace` may already have happened if directory fsync failed. Report that fact
        # only when a fresh content read matches this operation's candidate digest.
        try:
            with open(p,'rb') as f: replaced=sha(f.read())==candidate_sha
        except Exception: replaced=False
        fail('commit_failed',backup_path,replaced)
def read_backup(home,profile,path):
    directory=backup_dir(home)
    expected_dir=os.path.abspath(directory)
    absolute=os.path.abspath(path or '')
    if os.path.dirname(absolute)!=expected_dir or not re.fullmatch(r'(bashrc|zshrc)-[0-9a-f]{32}\.bak',os.path.basename(absolute)):
        fail('restore_conflict',path)
    try: st=os.lstat(absolute)
    except FileNotFoundError: fail('restore_conflict',path)
    if stat.S_ISLNK(st.st_mode) or not stat.S_ISREG(st.st_mode) or st.st_nlink!=1 or st.st_uid!=os.geteuid() or stat.S_IMODE(st.st_mode)!=0o600 or not metadata_safe(absolute):
        fail('restore_conflict',path)
    fd=os.open(absolute,os.O_RDONLY|getattr(os,'O_NOFOLLOW',0))
    try:
        opened=os.fstat(fd)
        if (opened.st_dev,opened.st_ino)!=(st.st_dev,st.st_ino): fail('restore_conflict',path)
        data=b''
        while True:
            chunk=os.read(fd,65536)
            if not chunk: break
            data+=chunk
            if len(data)>1048576: fail('restore_conflict',path)
    finally: os.close(fd)
    return data
def restore(home,profile,path,candidate_sha):
    old=read_backup(home,profile,path)
    p,st,current=read_target(home,profile)
    if sha(current)!=candidate_sha: fail('restore_conflict',path)
    tmp=None
    try:
        tmp=write_temp(p,st,old)
        # Conditional rollback is also best-effort; external edits remain possible before replace.
        _,now_st,now=read_target(home,profile)
        if sha(now)!=candidate_sha or (now_st.st_dev,now_st.st_ino)!=(st.st_dev,st.st_ino):
            fail('restore_conflict',path)
        os.replace(tmp,p); tmp=None
        fsync_dir(os.path.dirname(p))
    except Failure:
        if tmp:
            try: os.unlink(tmp)
            except OSError: pass
        raise
    except Exception:
        if tmp:
            try: os.unlink(tmp)
            except OSError: pass
        fail('restore_failed',path)
def snapshot(home):
    shell=os.path.basename(os.environ.get('SHELL',''))
    files={}
    if shell=='bash':
        for name in BASH_LOGIN_FILES:
            files[name]=read_startup(home,name)
            if files[name] is not None: break
        files['bashrc']=read_startup(home,'bashrc')
        files['bash_aliases']=read_startup(home,'bash_aliases',False)
    elif shell=='zsh':
        files={name:read_startup(home,name) for name in ZSH_FILES}
    return files
try:
    req=json.load(sys.stdin)
    home,uid=safe_home()
    if os.path.abspath(req.get('expected_home',''))!=home: fail('host_home_changed')
    check_shell_location(home)
    action=req.get('action')
    if action=='snapshot':
        out(ok=True,home=home,shell=os.environ.get('SHELL',''),home_uid=uid,files=snapshot(home))
    elif action=='prepare':
        backup,candidate_sha=prepare_backup(home,req.get('profile'),req.get('expected_sha256'),req.get('candidate','').encode('utf-8'),req.get('backup_path'))
        out(ok=True,backup_path=backup,candidate_sha256=candidate_sha)
    elif action=='verify_backup':
        data=read_backup(home,req.get('profile'),req.get('backup_path'))
        if sha(data)!=req.get('expected_sha256'): fail('backup_mismatch',req.get('backup_path'))
        out(ok=True,backup_path=req.get('backup_path'),candidate_sha256=sha(req.get('candidate','').encode('utf-8')))
    elif action=='commit':
        candidate_sha=commit_if_unchanged(home,req.get('profile'),req.get('expected_sha256'),req.get('candidate','').encode('utf-8'),req.get('backup_path'),req.get('candidate_sha256'))
        out(ok=True,backup_path=req.get('backup_path'),candidate_sha256=candidate_sha,replaced=True)
    elif action=='restore':
        restore(home,req.get('profile'),req.get('backup_path'),req.get('candidate_sha256'))
        out(ok=True)
    else: fail('source_unknown')
except Failure as e:
    out(ok=False,reason=e.reason,backup_path=e.backup_path,replaced=e.replaced)
except Exception:
    out(ok=False,reason='source_unknown')
"#;

async fn run_worker(
    fence: &hosts::HostFence,
    home: &str,
    req: &WorkerRequest<'_>,
) -> Result<WorkerReply, LcError> {
    let bytes = serde_json::to_vec(req).map_err(|_| LcError::Upstream("could not encode shell alias request".into()))?;
    let raw = if fence.conn().is_local() {
        let mut cmd = tokio::process::Command::new("python3");
        cmd.arg("-c")
            .arg(HOST_WORKER)
            .env("HOME", home)
            .env("SHELL", std::env::var("SHELL").unwrap_or_default())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|_| LcError::Upstream("local shell alias helper is unavailable".into()))?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt as _;
            stdin.write_all(&bytes).await.map_err(|_| LcError::Upstream("could not send local shell alias request".into()))?;
        }
        let output = tokio::time::timeout(HOST_IO_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| LcError::Upstream("local shell alias helper timed out".into()))?
            .map_err(|_| LcError::Upstream("local shell alias helper failed".into()))?;
        String::from_utf8(output.stdout).map_err(|_| LcError::Upstream("local shell alias helper returned invalid output".into()))?
    } else {
        let script = format!("python3 -c {}", hosts::sh_quote(HOST_WORKER));
        fence.conn().ssh_exec_stdin(&script, &bytes, HOST_IO_TIMEOUT).await
            .map_err(|_| LcError::Upstream("remote shell alias helper failed".into()))?
    };
    serde_json::from_str(&raw).map_err(|_| LcError::Upstream("shell alias helper returned invalid response".into()))
}

fn worker_reason(reason: &str) -> (&'static str, &'static str) {
    match reason {
        "source_unknown" | "unsupported_shell" | "source_unsupported" | "unsafe_file" | "host_home_changed" =>
            ("alias_source_unknown", "The shell startup source could not be verified safely."),
        "unsupported_zdotdir" => ("alias_source_unknown", "ZDOTDIR differs from HOME, so the zsh startup source cannot be verified safely."),
        "alias_missing" => ("alias_source_unknown", "The alias is no longer present in the verified startup source."),
        "alias_duplicate" => ("alias_definition_duplicate", "More than one direct definition matches this alias."),
        "alias_value_mismatch" => ("identity_source_mismatch", "The active shell alias does not match its startup-file definition."),
        "file_changed" => ("alias_file_changed", "The startup file changed while removal was being prepared."),
        "backup_failed" => ("alias_backup_failed", "A private retained backup could not be created."),
        "backup_mismatch" => ("alias_backup_mismatch", "The retained backup no longer matches the startup file snapshot."),
        "candidate_changed" => ("alias_candidate_changed", "The prepared replacement candidate changed before commit."),
        "syntax_check_failed" => ("alias_syntax_check_failed", "The candidate startup file did not pass the shell syntax check."),
        "commit_failed" => ("alias_write_failed", "The startup file could not be safely replaced."),
        "probe_failed" => ("alias_probe_failed", "The login shell could not verify the alias after replacement."),
        "alias_still_defined" => ("alias_still_defined", "The login shell still reports this alias after replacement."),
        "restore_conflict" | "restore_failed" => ("alias_restore_conflict", "The original startup file could not be conditionally restored safely."),
        _ => ("alias_source_unknown", "The shell startup source could not be verified safely."),
    }
}

fn worker_error(host: &str, identity: &str, reason: &str, backup_path: Option<&str>) -> LcError {
    let (code, message) = worker_reason(reason);
    conflict(host, identity, code, message, backup_path, None, None)
}

fn safe_meta(file: &WorkerFile, home_uid: u32) -> bool {
    !file.symlink && file.regular && file.uid == home_uid && file.nlink == 1 && file.mode & 0o7022 == 0
}

fn explicitly_references(kind: &str, bot_identity: Option<&str>, identity: &str) -> bool {
    kind == "claude" && bot_identity == Some(identity)
}

fn required_file<'a>(snapshot: &'a Snapshot, key: &str, host: &str, identity: &str) -> Result<&'a WorkerFile, LcError> {
    snapshot.files.get(key).and_then(Option::as_ref).ok_or_else(|| {
        conflict(host, identity, "alias_source_unknown", "The shell startup source could not be verified.", None, None, None)
    })
}

fn unsupported(host: &str, identity: &str) -> LcError {
    conflict(host, identity, "alias_definition_unsupported", "The startup file contains syntax outside the supported static settings and single-line alias subset.", None, None, None)
}

fn is_comment_or_blank(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

fn normalized_lines(text: &str) -> impl Iterator<Item = (usize, &str, &str)> {
    text.split_inclusive('\n').enumerate().map(|(index, raw)| {
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let line = line.strip_suffix('\r').unwrap_or(line);
        (index, raw, line)
    })
}

fn without_shell_comment(line: &str) -> &str {
    let mut quote = None;
    let mut previous_whitespace = true;
    for (index, ch) in line.char_indices() {
        if let Some(open) = quote {
            if ch == open { quote = None; }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
        } else if ch == '#' && previous_whitespace {
            return line[..index].trim_end();
        }
        previous_whitespace = ch.is_whitespace();
    }
    line
}

fn direct_alias_line(line: &str) -> Option<(String, String, char)> {
    let line = without_shell_comment(line).trim();
    let statement = line.strip_prefix("alias ")?;
    let (name, rest) = statement.split_once('=')?;
    if name.is_empty() || !name.as_bytes()[0].is_ascii_alphabetic() && name.as_bytes()[0] != b'_' {
        return None;
    }
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return None;
    }
    let (body, quote) = if let Some(body) = rest.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        (body, '\'')
    } else {
        (rest.strip_prefix('"').and_then(|v| v.strip_suffix('"'))?, '"')
    };
    if body.contains('\'') || body.contains('`') || body.contains("$(")
        || !body.bytes().all(|b| b.is_ascii_alphanumeric() || b" \t$#._/-=:+,@%*?!~".contains(&b))
    {
        return None;
    }
    Some((name.to_string(), body.to_string(), quote))
}

/// A small allowlist for ordinary, inert startup assignments. Values can contain only plain
/// characters and literal HOME/PATH references; command substitution, escaping, and arbitrary
/// shell statements remain unsupported so an unrecognized line cannot hide another source.
fn safe_setting_line(line: &str) -> bool {
    let line = without_shell_comment(line).trim();
    let assignment = line.strip_prefix("export ").unwrap_or(line);
    let Some((key, value)) = assignment.split_once('=') else { return false };
    let locale_key = key.strip_prefix("LC_").is_some_and(|tail| {
        !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    });
    if !matches!(key, "PATH"|"PS1"|"LANG"|"TERM") && !locale_key {
        return false;
    }
    if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        return !inner.contains('\'') && !inner.contains('\n');
    }
    let double_quoted = if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        let without_vars = inner.replace("${HOME}", "").replace("${PATH}", "").replace("$HOME", "").replace("$PATH", "");
        if without_vars.contains('$') { return false; }
        Some(inner)
    } else {
        None
    };
    if let Some(inner) = double_quoted {
        return inner.bytes().all(|b| b.is_ascii_alphanumeric() || b" $._/:+~-".contains(&b))
            && !inner.contains("$(") && !inner.contains('`') && !inner.contains(';');
    }
    let without_vars = value.replace("${HOME}", "").replace("${PATH}", "").replace("$HOME", "").replace("$PATH", "");
    !without_vars.contains('$') && without_vars.bytes().all(|b| b.is_ascii_alphanumeric() || b"_./:+~-".contains(&b))
}

fn probe_alias_line(line: &str, wanted: &str) -> Option<String> {
    let line = line.strip_prefix("alias ").unwrap_or(line);
    let prefix = format!("{wanted}=");
    let value = line.strip_prefix(&prefix)?;
    let body = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')).unwrap_or(value);
    (!body.is_empty() && !body.contains('\'') && valid_alias_body(body)).then(|| body.to_string())
}

fn valid_alias_body(body: &str) -> bool {
    let words: Vec<&str> = body.split_ascii_whitespace().collect();
    if words.is_empty() {
        return false;
    }
    let mut command_at = 0;
    if let Some(value) = words[0].strip_prefix("CLAUDE_CONFIG_DIR=") {
        let home_expansion_ok = !value.contains('$') || value == "$HOME" || (value.starts_with("$HOME/") && !value[6..].contains('$'));
        if value.is_empty()
            || !value.bytes().all(|b| b.is_ascii_alphanumeric() || b"$HOME/._-".contains(&b))
            || !home_expansion_ok
        {
            return false;
        }
        command_at = 1;
    }
    if words.get(command_at).is_none_or(|word| word.rsplit('/').next() != Some("claude")) {
        return false;
    }
    words[command_at..].iter().all(|word| {
        !word.is_empty()
            && word.bytes().all(|b| b.is_ascii_alphanumeric() || b"._/-=+".contains(&b))
    })
}

fn profile_uses_bashrc(text: &str, host: &str, identity: &str) -> Result<(), LcError> {
    let mut sources = 0;
    let mut in_guard = false;
    for (_, _, line) in normalized_lines(text) {
        if is_comment_or_blank(line) {
            continue;
        }
        let line = without_shell_comment(line).trim();
        if matches!(line,
            ". \"$HOME/.bashrc\"" | "source \"$HOME/.bashrc\"" |
            ". ~/.bashrc" | "source ~/.bashrc"
        ) {
            sources += 1;
        } else if !in_guard && matches!(line,
            "if [ -f \"$HOME/.bashrc\" ]; then" | "if [ -f ~/.bashrc ]; then"
        ) {
            in_guard = true;
        } else if in_guard && line == "fi" {
            in_guard = false;
        } else if safe_setting_line(line) && !in_guard {
            continue;
        } else {
            return Err(unsupported(host, identity));
        }
    }
    if sources == 1 && !in_guard {
        Ok(())
    } else {
        Err(conflict(host, identity, "alias_source_unknown", "The login shell does not have one verifiable direct .bashrc source.", None, None, None))
    }
}

fn only_comments(text: &str, host: &str, identity: &str) -> Result<(), LcError> {
    if normalized_lines(text).all(|(_, _, line)| is_comment_or_blank(line)) {
        Ok(())
    } else {
        Err(unsupported(host, identity))
    }
}

struct Candidate {
    profile: &'static str,
    original: String,
    replacement: String,
}

fn source_candidate(snapshot: &Snapshot, name: &str, probe: &str, host: &str) -> Result<Candidate, LcError> {
    let shell_path = std::path::Path::new(&snapshot.shell);
    let shell = shell_path.file_name().and_then(|x| x.to_str()).unwrap_or("");
    if !shell_path.is_absolute() {
        return Err(conflict(host, name, "alias_source_unknown", "The host did not report an absolute login-shell path.", None, None, None));
    }
    let (profile, target_file) = match shell {
        "bash" => {
            let selected = ["bash_profile", "bash_login", "profile"]
                .into_iter()
                .find(|key| snapshot.files.get(*key).is_some_and(Option::is_some))
                .ok_or_else(|| conflict(host, name, "alias_source_unknown", "The login shell has no verifiable startup file.", None, None, None))?;
            let startup = required_file(snapshot, selected, host, name)?;
            if !safe_meta(startup, snapshot.home_uid) {
                return Err(conflict(host, name, "alias_source_unknown", "A login startup file has unsafe ownership or permissions.", None, None, None));
            }
            profile_uses_bashrc(startup.text.as_deref().ok_or_else(|| unsupported(host, name))?, host, name)?;
            if snapshot.files.get("bash_aliases").is_some_and(Option::is_some) {
                return Err(conflict(host, name, "alias_source_unknown", "A separate .bash_aliases source makes this alias origin ambiguous.", None, None, None));
            }
            ("bashrc", required_file(snapshot, "bashrc", host, name)?)
        }
        "zsh" => {
            for key in ["zshenv", "zprofile", "zlogin"] {
                if let Some(file) = snapshot.files.get(key).and_then(Option::as_ref) {
                    if !safe_meta(file, snapshot.home_uid) {
                        return Err(conflict(host, name, "alias_source_unknown", "A login startup file has unsafe ownership or permissions.", None, None, None));
                    }
                    only_comments(file.text.as_deref().ok_or_else(|| unsupported(host, name))?, host, name)?;
                }
            }
            ("zshrc", required_file(snapshot, "zshrc", host, name)?)
        }
        _ => return Err(conflict(host, name, "alias_source_unknown", "Only bash and zsh startup sources are supported.", None, None, None)),
    };
    if !safe_meta(target_file, snapshot.home_uid) {
        return Err(conflict(host, name, "alias_source_unknown", "The candidate startup file has unsafe ownership or permissions.", None, None, None));
    }
    let original = target_file.text.clone().ok_or_else(|| unsupported(host, name))?;
    let mut matches = Vec::new();
    for (index, raw, line) in normalized_lines(&original) {
        if is_comment_or_blank(line) {
            continue;
        }
        if let Some((defined_name, body, quote)) = direct_alias_line(line) {
            if defined_name == name && (quote != '\'' || !valid_alias_body(&body)) {
                return Err(unsupported(host, name));
            }
            if defined_name == name {
                matches.push((index, raw, body));
            }
        } else if safe_setting_line(line) {
            continue;
        } else {
            return Err(unsupported(host, name));
        }
    }
    if matches.is_empty() {
        return Err(conflict(host, name, "alias_source_unknown", "No direct definition was found in the supported startup file.", None, None, None));
    }
    if matches.len() != 1 {
        return Err(conflict(host, name, "alias_definition_duplicate", "More than one direct definition matches this alias.", None, None, None));
    }
    let (index, raw, body) = &matches[0];
    match parse_probe(probe, name, host)? {
        Some(active) if active == *body => {}
        Some(_) => return Err(conflict(host, name, "identity_source_mismatch", "The active shell alias does not match its startup-file definition.", None, None, None)),
        None => return Err(conflict(host, name, "alias_source_unknown", "The login shell did not report the expected alias.", None, None, None)),
    }
    let start = normalized_lines(&original).take(*index).map(|(_, raw, _)| raw.len()).sum::<usize>();
    let end = start + raw.len();
    let replacement = format!("{}{}", &original[..start], &original[end..]);
    Ok(Candidate { profile, original, replacement })
}

fn parse_probe(output: &str, name: &str, host: &str) -> Result<Option<String>, LcError> {
    let mut body = None;
    let alias_prefix = format!("alias {name}=");
    let bare_prefix = format!("{name}=");
    for line in output.lines() {
        if line.starts_with(&alias_prefix) || line.starts_with(&bare_prefix) {
            let parsed = probe_alias_line(line, name).ok_or_else(|| unsupported(host, name))?;
            if !valid_alias_body(&parsed) {
                return Err(unsupported(host, name));
            }
            if body.replace(parsed).is_some() {
                return Err(conflict(host, name, "alias_definition_duplicate", "The login shell reported duplicate alias definitions.", None, None, None));
            }
        }
    }
    Ok(body)
}

fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

async fn probe(fence: &hosts::HostFence, home: &str, shell: &str, name: &str, host: &str) -> Result<String, LcError> {
    let probe_script = format!("alias {name}; printf '%s\\n' '{PROBE_SENTINEL}'");
    let command = format!(
        "HOME={} SHELL={} {} -lic {} 2>/dev/null",
        hosts::sh_quote(home),
        hosts::sh_quote(shell),
        hosts::sh_quote(shell),
        hosts::sh_quote(&probe_script),
    );
    let output = if fence.conn().is_local() {
        hosts::sh_local_stdout(&command, Duration::from_secs(8), "shell alias probe")
            .await
            .map_err(|_| conflict(host, name, "alias_probe_failed", "The login shell could not be queried safely.", None, None, None))?
    } else {
        fence.conn().ssh_exec_timeout(&command, Duration::from_secs(8)).await
            .map_err(|_| conflict(host, name, "alias_probe_failed", "The host login shell could not be queried safely.", None, None, None))?
    };
    if !output.lines().any(|line| line == PROBE_SENTINEL) {
        return Err(conflict(host, name, "alias_probe_failed", "The login shell did not complete the alias verification command.", None, None, None));
    }
    Ok(output.lines().filter(|line| *line != PROBE_SENTINEL).collect::<Vec<_>>().join("\n"))
}

async fn conditional_restore(
    fence: &hosts::HostFence,
    home: &str,
    profile: &str,
    backup: &str,
    candidate_sha: &str,
) -> bool {
    let reply = run_worker(fence, home, &WorkerRequest {
        action: "restore", profile: Some(profile), expected_sha256: None, candidate: None,
        backup_path: Some(backup), candidate_sha256: Some(candidate_sha), expected_home: home,
    }).await;
    matches!(reply, Ok(WorkerReply { ok: true, .. }))
}

async fn delete_on_host(app: &Arc<App>, fence: &hosts::HostFence, host: &str, name: &str) -> LcResult<Response> {
    let config = app.cfg.get().await;
    let cached_shells = app.tools.lock().await.get(host).map(|tools| tools.shell_identities.clone());
    let resolved = crate::tools::merge_identities(&config.identities, host, cached_shells.as_deref());
    if let Some((_, source)) = resolved.iter().find(|(entry, _)| entry.name == name && entry.kind == "claude") {
        if *source != crate::tools::SOURCE_SHELL {
            return Err(conflict(host, name, "identity_source_mismatch", "A config identity with this name takes precedence; this endpoint only removes shell aliases.", None, None, None));
        }
    } else {
        return Err(conflict(host, name, "alias_source_unknown", "The current host snapshot does not identify this shell alias.", None, None, None));
    }

    if let Some(reference) = first_reference(app, host, name)
        .await
        .map_err(|_| LcError::Unavailable(json!({
            "error":"identity_reference_check_failed","reason":"identity_reference_check_failed",
            "host":host,"kind":"claude","identity":name,"retryable":true,"retry_after_secs":10,
            "message":"Could not verify identity references; no files were changed."
        })))?
    {
        return Err(conflict(host, name, "identity_in_use", "A Bot or supervisor role still explicitly references this identity.", None, Some(vec![reference]), None));
    }

    let home = hosts::home_for_fence(fence).await.map_err(|_| {
        conflict(host, name, "alias_source_unknown", "The target host home directory is unavailable.", None, None, None)
    })?;
    let snapshot = run_worker(fence, &home, &WorkerRequest {
        action: "snapshot", profile: None, expected_sha256: None, candidate: None,
        backup_path: None, candidate_sha256: None, expected_home: &home,
    }).await.map_err(|_| conflict(host, name, "alias_source_unknown", "The host startup files could not be inspected safely.", None, None, None))?
        .snapshot(host, name)?;
    if snapshot.home != home {
        return Err(conflict(host, name, "alias_source_unknown", "The host home directory changed during inspection.", None, None, None));
    }
    let active = probe(fence, &home, &snapshot.shell, name, host).await?;
    let candidate = source_candidate(&snapshot, name, &active, host)?;
    let expected = digest(&candidate.original);
    // Keep the intended backup location even if transport loses the prepare response.
    let token = digest(&ulid::Ulid::new().to_string());
    let intended_backup = format!("{home}/.config/agents-manager/shell-alias-backups/{}-{}.bak", candidate.profile, &token[..32]);
    let prepared = match run_worker(fence, &home, &WorkerRequest {
        action: "prepare", profile: Some(candidate.profile), expected_sha256: Some(&expected),
        candidate: Some(&candidate.replacement), backup_path: Some(&intended_backup), candidate_sha256: None, expected_home: &home,
    }).await {
        Ok(reply) => reply,
        Err(_) => {
            // Reconcile by reading this exact backup, never retry a file write blindly.
            match run_worker(fence, &home, &WorkerRequest {
                action: "verify_backup", profile: Some(candidate.profile), expected_sha256: Some(&expected),
                candidate: Some(&candidate.replacement), backup_path: Some(&intended_backup), candidate_sha256: None, expected_home: &home,
            }).await {
                Ok(reply) if reply.ok => reply,
                _ => return Err(conflict(host, name, "alias_backup_unconfirmed", "The backup could not be confirmed; the startup file was not changed. The intended backup location is provided for inspection.", Some(&intended_backup), None, None)),
            }
        }
    };
    if !prepared.ok {
        return Err(worker_error(host, name, prepared.reason.as_deref().unwrap_or("backup_failed"), prepared.backup_path.as_deref()));
    }
    let backup_path = prepared.backup_path.ok_or_else(|| conflict(host, name, "alias_backup_failed", "The retained backup path was not confirmed; the startup file was not changed.", None, None, None))?;
    let candidate_sha = prepared.candidate_sha256.unwrap_or_else(|| digest(&candidate.replacement));

    let committed = match run_worker(fence, &home, &WorkerRequest {
        action: "commit", profile: Some(candidate.profile), expected_sha256: Some(&expected),
        candidate: Some(&candidate.replacement), backup_path: Some(&backup_path), candidate_sha256: Some(&candidate_sha), expected_home: &home,
    }).await {
        Ok(reply) => reply,
        Err(_) => {
            // The transport may have lost the reply after the helper renamed the file. Restore
            // only if its current bytes still match this candidate; include the confirmed backup.
            let restored = conditional_restore(fence, &home, candidate.profile, &backup_path, &candidate_sha).await;
            let (reason, message) = if restored {
                ("alias_commit_outcome_unknown", "The commit result was not received; conditional restore completed from the retained backup.")
            } else {
                ("alias_restore_conflict", "The commit result was not received; conditional restore could not safely replace the current file.")
            };
            return Err(conflict(host, name, reason, message, Some(&backup_path), None, Some(restored)));
        }
    };
    if !committed.ok {
        let (reason, message) = worker_reason(committed.reason.as_deref().unwrap_or("commit_failed"));
        let restored = if committed.replaced {
            Some(conditional_restore(fence, &home, candidate.profile, &backup_path, &candidate_sha).await)
        } else { None };
        if restored == Some(false) {
            return Err(conflict(host, name, "alias_restore_conflict", "The write failed after replacement, and conditional restore could not safely replace the current file.", Some(&backup_path), None, restored));
        }
        return Err(conflict(host, name, reason, message, Some(&backup_path), None, restored));
    }

    match probe(fence, &home, &snapshot.shell, name, host).await {
        Ok(output) => match parse_probe(&output, name, host) {
            Ok(None) => (),
            Ok(Some(_)) => {
                let restored = conditional_restore(fence, &home, candidate.profile, &backup_path, &candidate_sha).await;
                let (reason, message) = if restored {
                    ("alias_still_defined", "The login shell still reports this alias; the original file was conditionally restored.")
                } else {
                    ("alias_restore_conflict", "The login shell still reports this alias, and conditional restore could not safely replace the current file.")
                };
                return Err(conflict(host, name, reason, message, Some(&backup_path), None, Some(restored)));
            }
            Err(_) => {
                let restored = conditional_restore(fence, &home, candidate.profile, &backup_path, &candidate_sha).await;
                let (reason, message) = if restored {
                    ("alias_probe_failed", "The post-write login-shell output could not be verified; the original file was conditionally restored.")
                } else {
                    ("alias_restore_conflict", "The post-write login-shell output could not be verified, and conditional restore could not safely replace the current file.")
                };
                return Err(conflict(host, name, reason, message, Some(&backup_path), None, Some(restored)));
            }
        },
        Err(_) => {
            let restored = conditional_restore(fence, &home, candidate.profile, &backup_path, &candidate_sha).await;
            let (reason, message) = if restored {
                ("alias_probe_failed", "The post-write login-shell check failed; the original file was conditionally restored.")
            } else {
                ("alias_restore_conflict", "The post-write login-shell check failed, and conditional restore could not safely replace the current file.")
            };
            return Err(conflict(host, name, reason, message, Some(&backup_path), None, Some(restored)));
        }
    };

    match first_reference(app, host, name).await {
        Ok(Some(reference)) => {
            let restored = conditional_restore(fence, &home, candidate.profile, &backup_path, &candidate_sha).await;
            let (reason, message) = if restored {
                ("alias_new_reference", "A matching identity reference appeared during removal; the original file was conditionally restored.")
            } else {
                ("alias_restore_conflict", "A matching identity reference appeared during removal, and conditional restore could not safely replace the current file.")
            };
            return Err(conflict(host, name, reason, message, Some(&backup_path), Some(vec![reference]), Some(restored)));
        }
        Err(_) => {
            let restored = conditional_restore(fence, &home, candidate.profile, &backup_path, &candidate_sha).await;
            let (reason, message) = if restored {
                ("identity_reference_check_failed", "Identity references could not be rechecked; the original file was conditionally restored.")
            } else {
                ("alias_restore_conflict", "Identity references could not be rechecked, and conditional restore could not safely replace the current file.")
            };
            return Err(conflict(host, name, reason, message, Some(&backup_path), None, Some(restored)));
        }
        Ok(None) => {}
    }

    // Take a new host observation ticket after the file replacement. This prevents a slower
    // pre-delete alias poll from publishing its stale snapshot over the deletion result.
    let publish_fence = app.hosts.fence(host).await.ok_or_else(|| {
        conflict(host, name, "host_superseded", "The alias was removed but the host cache could not be refreshed.", Some(&backup_path), None, None)
    })?;
    if !publish_fence.same_authority(fence) {
        return Err(conflict(host, name, "host_superseded", "The alias was removed but the host authority changed before cache refresh.", Some(&backup_path), None, None));
    }
    {
        let mut cache = app.tools.lock().await;
        if publish_fence.claim_publish() {
            if let Some(tools) = cache.get_mut(host) {
                tools.shell_identities.retain(|entry| !(entry.name == name && entry.kind == "claude"));
                if tools.identities.get(name).is_some_and(|entry| entry.kind == "claude" && entry.source == crate::tools::SOURCE_SHELL) {
                    tools.identities.remove(name);
                }
            }
        }
    }
    crate::state::emit_host_changed(app, &publish_fence).await;
    Ok((StatusCode::OK, Json(json!({
        "ok":true,"host":host,"kind":"claude","identity":name,"backup_path":backup_path,
        "warning":"external_editor_window"
    }))).into_response())
}

#[derive(Debug, Deserialize)]
pub(super) struct DeleteQuery {
    kind: Option<String>,
}

pub(super) async fn delete_shell_identity(
    State(app): State<Arc<App>>,
    Path((host, identity)): Path<(String, String)>,
    Query(query): Query<DeleteQuery>,
    Extension(principal): Extension<RequestPrincipal>,
) -> LcResult<Response> {
    if principal != RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error":"forbidden","reason":"user_only","host":host,"kind":"claude","identity":identity})));
    }
    if query.kind.as_deref() != Some("claude") || !crate::config::valid_host_name(&host)
        || !matches!(identity.as_str(), "cc0"|"cc1"|"cc2"|"cc3"|"cc4"|"cc5"|"cc6")
    {
        return Err(invalid_identity(&host, &identity, "Shell identity deletion requires kind=claude and a valid host cc0..cc6."));
    }
    let lock = host_lock(&host);
    let _host_guard = lock.lock().await;
    let fence = app.hosts.fence(&host).await.ok_or_else(|| LcError::NotFoundValue(json!({
        "error":"not_found","reason":"host_not_found","host":host,"kind":"claude","identity":identity
    })))?;
    match fence.run_current(delete_on_host(&app, &fence, &host, &identity)).await {
        Some(result) => result,
        None => Err(conflict(&host, &identity, "host_superseded", "The host connection changed before this operation began.", None, None, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::process::{Command, Stdio};

    struct TempHome(std::path::PathBuf);

    impl TempHome {
        fn new() -> Self {
            let path = crate::testing::scratch_dir("agm-shell-alias-delete");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(fs::canonicalize(&path).unwrap(), path, "test HOME must pass production's no-symlink check");
            Self(path)
        }

        fn write(&self, name: &str, text: &str) {
            let path = self.0.join(format!(".{name}"));
            fs::write(&path, text).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn run_fake_worker_with_zdot(home: &TempHome, shell: &str, zdotdir: Option<&std::path::Path>, request: Value) -> WorkerReply {
        let mut command = Command::new("python3");
        command.arg("-c").arg(HOST_WORKER)
            .env("HOME", &home.0)
            .env("SHELL", shell)
            .env_remove("ZDOTDIR").env_remove("BASH_ENV").env_remove("ENV")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(zdotdir) = zdotdir { command.env("ZDOTDIR", zdotdir); }
        let mut child = command.spawn().expect("python3 for isolated temp HOME worker test");
        child.stdin.take().unwrap().write_all(&serde_json::to_vec(&request).unwrap()).unwrap();
        let output = child.wait_with_output().unwrap();
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn run_fake_worker(home: &TempHome, shell: &str, request: Value) -> WorkerReply {
        run_fake_worker_with_zdot(home, shell, None, request)
    }

    fn request(home: &TempHome, action: &str, more: Value) -> Value {
        let mut value = json!({"action":action,"expected_home":home.0.display().to_string()});
        value.as_object_mut().unwrap().extend(more.as_object().unwrap().clone());
        value
    }

    fn fake_zsh(home: &TempHome, fail_syntax: bool) -> std::path::PathBuf {
        let bin = home.0.join("bin");
        fs::create_dir(&bin).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
        let shell = bin.join("zsh");
        let body = if fail_syntax { "echo PRIVATE_SYNTAX_ERROR >&2; exit 2" } else { "exit 0" };
        crate::testing::write_exec(&shell, format!("#!/bin/sh\nif [ \"$1\" = \"-n\" ]; then {body}; fi\nexit 0\n"));
        shell
    }

    struct FakeRemote {
        env: crate::testing::Env,
        home: TempHome,
        host: String,
        project_id: String,
        worker_calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    struct ProbePause {
        reached: std::sync::mpsc::Receiver<()>,
        release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    impl ProbePause {
        async fn wait(&mut self) {
            let reached = std::mem::replace(&mut self.reached, std::sync::mpsc::channel().1);
            tokio::task::spawn_blocking(move || reached.recv_timeout(std::time::Duration::from_secs(5)).unwrap())
                .await.unwrap();
        }

        fn release(&self) {
            let (lock, wake) = &*self.release;
            *lock.lock().unwrap() = true;
            wake.notify_all();
        }
    }

    impl Drop for ProbePause {
        fn drop(&mut self) { self.release(); }
    }

    async fn fake_remote(pause_when_alias_present: Option<bool>, lose_commit_reply: bool) -> (FakeRemote, Option<ProbePause>) {
        fake_remote_with_loss(pause_when_alias_present, lose_commit_reply, false).await
    }

    async fn fake_remote_with_loss(pause_when_alias_present: Option<bool>, lose_commit_reply: bool, lose_prepare_reply: bool) -> (FakeRemote, Option<ProbePause>) {
        fake_remote_with_faults(pause_when_alias_present, lose_commit_reply, lose_prepare_reply, false).await
    }

    async fn fake_remote_with_faults(pause_when_alias_present: Option<bool>, lose_commit_reply: bool, lose_prepare_reply: bool, fail_commit_then_edit: bool) -> (FakeRemote, Option<ProbePause>) {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let env = crate::testing::env().await;
        let home = TempHome::new();
        let shell = fake_zsh(&home, false);
        let body = "CLAUDE_CONFIG_DIR=$HOME/.claude-cc0 claude --dangerously-skip-permissions";
        home.write("zshenv", "# fake zsh startup\n");
        home.write("zshrc", &format!("# keep remote\nalias cc0='{body}'\nalias ll='ls -alF'\n"));
        let host = format!("m4p-{}", ulid::Ulid::new().to_string().to_ascii_lowercase());
        let project_id = crate::db::ulid();
        sqlx::query("INSERT INTO projects (id,path,label,host,created_at) VALUES (?, '/tmp/fake-remote', 'fake-remote', ?, ?)")
            .bind(&project_id).bind(&host).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        env.app.hosts.insert_remote_for_test(crate::config::HostCfg {
            name: host.clone(), ssh: "unused-fake-transport".into(), ssh_port: 22,
            ssh_opts: vec![], herdr_session: "fake".into(), remote_path: String::new(), shared_session: false,
        }).await;
        let identity = crate::config::IdentityCfg {
            name: "cc0".into(), kind: "claude".into(), host: None,
            env: Default::default(), args: vec![],
        };
        env.app.tools.lock().await.insert(host.clone(), crate::tools::HostTools {
            tools: Default::default(), identities: Default::default(), shell_identities: vec![identity],
            utc_offset_secs: None, herdr_cli: None, checked_at: crate::db::now(),
        });

        let home_path = home.0.clone();
        let home_text = home.0.display().to_string();
        let did_pause = Arc::new(AtomicBool::new(false));
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let (tx, rx) = std::sync::mpsc::channel();
        let pause_gate = release.clone();
        let pause_seen = did_pause.clone();
        hosts::set_ssh_fake(&host, move |script| {
            if script.contains("printf '%s' \"$HOME\"") {
                return Ok(home_text.clone());
            }
            if script.contains(PROBE_SENTINEL) {
                let content = fs::read_to_string(home_path.join(".zshrc")).unwrap_or_default();
                let alias = content.lines().find(|line| line.trim_start().starts_with("alias cc0="));
                let has_alias = alias.is_some();
                if pause_when_alias_present == Some(has_alias) && !pause_seen.swap(true, Ordering::SeqCst) {
                    let _ = tx.send(());
                    let (lock, wake) = &*pause_gate;
                    let mut ready = lock.lock().unwrap();
                    while !*ready { ready = wake.wait(ready).unwrap(); }
                }
                return Ok(format!("{}{}\n", alias.map(|line| format!("{line}\n")).unwrap_or_default(), PROBE_SENTINEL));
            }
            anyhow::bail!("unexpected fake SSH command")
        });

        let worker_calls = Arc::new(AtomicUsize::new(0));
        let calls = worker_calls.clone();
        let shell_text = shell.display().to_string();
        let worker_home = home.0.clone();
        hosts::set_ssh_fake_io(&host, move |_script, input| {
            calls.fetch_add(1, Ordering::SeqCst);
            let action = serde_json::from_slice::<Value>(input)?["action"].as_str().unwrap_or("").to_string();
            let mut child = Command::new("python3")
                .arg("-c").arg(HOST_WORKER)
                .env("HOME", &worker_home)
                .env("SHELL", &shell_text)
                .env_remove("ZDOTDIR").env_remove("BASH_ENV").env_remove("ENV")
                .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
                .spawn()?;
            child.stdin.take().unwrap().write_all(input)?;
            let output = child.wait_with_output()?;
            if !output.status.success() { anyhow::bail!("fake host worker failed"); }
            if fail_commit_then_edit && action == "commit" {
                fs::write(worker_home.join(".zshrc"), "# external edit must survive\n")?;
                return Ok(serde_json::to_vec(&json!({"ok":false,"reason":"commit_failed","replaced":true}))?);
            }
            if lose_prepare_reply && action == "prepare" { anyhow::bail!("fake transport lost prepare response"); }
            if lose_commit_reply && action == "commit" { anyhow::bail!("fake transport lost commit response"); }
            Ok(output.stdout)
        });
        let pause = pause_when_alias_present.map(|_| ProbePause { reached: rx, release });
        (FakeRemote { env, home, host, project_id, worker_calls }, pause)
    }

    async fn call_delete(app: Arc<App>, host: String, principal: RequestPrincipal) -> LcResult<Response> {
        delete_shell_identity(
            State(app), Path((host, "cc0".into())), Query(DeleteQuery { kind: Some("claude".into()) }),
            Extension(principal),
        ).await
    }

    async fn error_body(error: LcError) -> (StatusCode, Value) {
        let response = error.into_response();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    async fn add_remote_claude_ref(remote: &FakeRemote, name: &str) -> crate::db::Bot {
        let bot = crate::testing::claude_bot(&remote.env.app, &remote.project_id, name).await;
        sqlx::query("UPDATE bots SET identity='cc0' WHERE id=?").bind(&bot.id).execute(&remote.env.app.db).await.unwrap();
        bot
    }

    fn file(text: &str) -> WorkerFile {
        WorkerFile { text: Some(text.to_string()), symlink: false, regular: true, mode: 0o600, uid: unsafe { libc::geteuid() } as u32, nlink: 1 }
    }

    #[test]
    fn local_bash_and_m4p_zsh_candidates_are_strict_and_byte_preserving() {
        let bash_body = "CLAUDE_CONFIG_DIR=$HOME/.claude-cc1 claude --dangerously-skip-permissions";
        let mut bash_files = BTreeMap::new();
        bash_files.insert("bash_profile".into(), Some(file("# login\nexport PATH=\"$PATH:$HOME/bin\"\nif [ -f \"$HOME/.bashrc\" ]; then\n  . \"$HOME/.bashrc\"\nfi\n")));
        let original_bash = format!("# keep\nexport PATH=\"$PATH:$HOME/tools\" # keep PATH comment\nexport PS1='\\u@\\h:\\w\\$ '\nalias ll='ls -alF' # keep alias comment\nalias gst=\"git status\" # keep double quote alias\nalias cc2='claude'\nalias cc1='{bash_body}'\n");
        bash_files.insert("bashrc".into(), Some(file(&original_bash)));
        bash_files.insert("bash_aliases".into(), None);
        let bash = Snapshot { home: "/fake".into(), shell: "/bin/bash".into(), home_uid: unsafe { libc::geteuid() } as u32, files: bash_files };
        let got = source_candidate(&bash, "cc1", &format!("alias cc1='{bash_body}'\n"), "local").unwrap();
        assert_eq!(got.profile, "bashrc");
        assert_eq!(got.replacement, original_bash.replace(&format!("alias cc1='{bash_body}'\n"), ""), "unrelated assignments and aliases keep their exact bytes");

        let zsh_body = "CLAUDE_CONFIG_DIR=$HOME/.claude-cc2 claude --dangerously-skip-permissions";
        let mut zsh_files = BTreeMap::new();
        zsh_files.insert("zshenv".into(), Some(file("# empty\n")));
        zsh_files.insert("zprofile".into(), None);
        zsh_files.insert("zlogin".into(), None);
        zsh_files.insert("zshrc".into(), Some(file(&format!("alias cc2='{zsh_body}'\n"))));
        let zsh = Snapshot { home: "/fake".into(), shell: "/bin/zsh".into(), home_uid: unsafe { libc::geteuid() } as u32, files: zsh_files };
        assert_eq!(source_candidate(&zsh, "cc2", &format!("cc2='{zsh_body}'\n"), "m4p").unwrap().profile, "zshrc");
    }

    #[test]
    fn duplicate_generated_or_unverifiable_source_is_refused() {
        let mut files = BTreeMap::new();
        files.insert("bash_profile".into(), Some(file(". \"$HOME/.bashrc\"\n")));
        files.insert("bashrc".into(), Some(file("alias cc0='claude'\nalias cc0='claude --foo'\n")));
        files.insert("bash_aliases".into(), None);
        let snapshot = Snapshot { home: "/fake".into(), shell: "/bin/bash".into(), home_uid: unsafe { libc::geteuid() } as u32, files };
        assert!(matches!(source_candidate(&snapshot, "cc0", "alias cc0='claude --foo'\n", "local"), Err(LcError::Conflict(v)) if v["reason"] == "alias_definition_duplicate"));

        let mut generated = snapshot;
        generated.files.insert("bashrc".into(), Some(file("for n in cc0; do alias $n='claude'; done\n")));
        assert!(source_candidate(&generated, "cc0", "alias cc0='claude'\n", "local").is_err());

        let mut double_quoted_files = BTreeMap::new();
        double_quoted_files.insert("bash_profile".into(), Some(file(". \"$HOME/.bashrc\"\n")));
        double_quoted_files.insert("bashrc".into(), Some(file("alias cc0=\"claude\"\n")));
        double_quoted_files.insert("bash_aliases".into(), None);
        let double_quoted = Snapshot { home: "/fake".into(), shell: "/bin/bash".into(), home_uid: unsafe { libc::geteuid() } as u32, files: double_quoted_files };
        assert!(source_candidate(&double_quoted, "cc0", "alias cc0='claude'\n", "local").is_err(), "non-target static aliases may be preserved, but target grammar stays single-quoted and strict");
    }

    #[test]
    fn only_an_explicit_same_kind_identity_is_a_bot_reference() {
        assert!(!explicitly_references("claude", None, "cc0"));
        assert!(!explicitly_references("claude", Some("cc1"), "cc0"));
        assert!(!explicitly_references("codex", Some("cc0"), "cc0"));
        assert!(explicitly_references("claude", Some("cc0"), "cc0"));
    }

    #[test]
    fn macos_local_worker_backup_and_conditional_restore_use_only_temp_home_and_keep_backup() {
        let home = TempHome::new();
        let original = "# fake rc\nalias cc0='claude --dangerously-skip-permissions'\n";
        home.write("bashrc", original);
        let snap = run_fake_worker(&home, "/bin/bash", request(&home, "snapshot", json!({})));
        assert!(snap.ok);
        let old_sha = digest(original);
        let replacement = "# fake rc\n";
        let prepared = run_fake_worker(&home, "/bin/bash", request(&home, "prepare", json!({
            "profile":"bashrc","expected_sha256":old_sha,"candidate":replacement
        })));
        assert!(prepared.ok);
        let backup = prepared.backup_path.unwrap();
        assert_eq!(prepared.candidate_sha256, Some(digest(replacement)));
        assert_eq!(fs::read_to_string(home.0.join(".bashrc")).unwrap(), original, "prepare confirms a backup but does not edit the rc file");
        let backup_meta = fs::metadata(&backup).unwrap();
        assert_eq!(backup_meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_to_string(&backup).unwrap(), original);

        let commit = run_fake_worker(&home, "/bin/bash", request(&home, "commit", json!({
            "profile":"bashrc","expected_sha256":old_sha,"candidate":replacement,
            "backup_path":backup,"candidate_sha256":digest(replacement)
        })));
        assert!(commit.ok, "{commit:?}");
        assert!(commit.replaced);
        assert_eq!(fs::read_to_string(home.0.join(".bashrc")).unwrap(), replacement);

        let restored = run_fake_worker(&home, "/bin/bash", request(&home, "restore", json!({
            "profile":"bashrc","backup_path":backup,"candidate_sha256":digest(replacement)
        })));
        assert!(restored.ok);
        assert_eq!(fs::read_to_string(home.0.join(".bashrc")).unwrap(), original);
        assert!(fs::metadata(&backup).is_ok(), "a successful restore must retain the original backup");
        assert_eq!(fs::metadata(home.0.join(".bashrc")).unwrap().uid(), unsafe { libc::geteuid() });
    }

    #[test]
    fn macos_local_conditional_restore_refuses_external_temp_file_edit() {
        let home = TempHome::new();
        let original = "alias cc3='claude'\n";
        home.write("zshrc", original);
        let shell = fake_zsh(&home, false);
        let prepared = run_fake_worker(&home, shell.to_str().unwrap(), request(&home, "prepare", json!({
            "profile":"zshrc","expected_sha256":digest(original),"candidate":"# removed\n"
        })));
        assert!(prepared.ok);
        let backup = prepared.backup_path.unwrap();
        let commit = run_fake_worker(&home, shell.to_str().unwrap(), request(&home, "commit", json!({
            "profile":"zshrc","expected_sha256":digest(original),"candidate":"# removed\n",
            "backup_path":backup,"candidate_sha256":digest("# removed\n")
        })));
        assert!(commit.ok);
        fs::write(home.0.join(".zshrc"), "# external edit\n").unwrap();
        let rollback = run_fake_worker(&home, shell.to_str().unwrap(), request(&home, "restore", json!({
            "profile":"zshrc","backup_path":backup,"candidate_sha256":digest("# removed\n")
        })));
        assert!(!rollback.ok);
        assert_eq!(rollback.reason.as_deref(), Some("restore_conflict"));
        assert_eq!(fs::read_to_string(home.0.join(".zshrc")).unwrap(), "# external edit\n");
        assert!(fs::metadata(backup).is_ok());
    }

    #[test]
    fn macos_local_backup_directory_and_file_with_extra_metadata_are_rejected() {
        let home = TempHome::new();
        let original = "alias cc0='claude'\n";
        home.write("bashrc", original);
        let directory = home.0.join(".config/agents-manager/shell-alias-backups");
        fs::create_dir_all(&directory).unwrap();
        for part in [".config", ".config/agents-manager", ".config/agents-manager/shell-alias-backups"] {
            fs::set_permissions(home.0.join(part), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let xattr = |path: &std::path::Path, remove: bool| {
            let script = "import os,sys; name='com.agm.test' if sys.platform=='darwin' else 'user.agm.test'; os.removexattr(sys.argv[1],name) if sys.argv[2]=='remove' else os.setxattr(sys.argv[1],name,b'temp-test')";
            assert!(Command::new("python3").arg("-c").arg(script).arg(path).arg(if remove { "remove" } else { "set" }).status().unwrap().success());
        };
        let unsafe_home = TempHome::new();
        unsafe_home.write("bashrc", original);
        xattr(&unsafe_home.0, false);
        let rejected = run_fake_worker(&unsafe_home, "/bin/bash", request(&unsafe_home, "prepare", json!({
            "profile":"bashrc","expected_sha256":digest(original),"candidate":"# removed\n"
        })));
        assert!(!rejected.ok, "HOME metadata must be checked before creating a backup");
        assert!(!unsafe_home.0.join(".config").exists());
        xattr(&directory, false);
        let prepare = || run_fake_worker(&home, "/bin/bash", request(&home, "prepare", json!({
            "profile":"bashrc","expected_sha256":digest(original),"candidate":"# removed\n"
        })));
        assert!(!prepare().ok, "metadata on the private directory must block backup confirmation");
        xattr(&directory, true);
        let prepared = prepare();
        assert!(prepared.ok, "{prepared:?}");
        let backup = prepared.backup_path.unwrap();
        xattr(std::path::Path::new(&backup), false);
        let commit = run_fake_worker(&home, "/bin/bash", request(&home, "commit", json!({
            "profile":"bashrc","expected_sha256":digest(original),"candidate":"# removed\n",
            "backup_path":backup,"candidate_sha256":digest("# removed\n")
        })));
        assert!(!commit.ok, "metadata on the backup file must block reuse");
        assert_eq!(fs::read_to_string(home.0.join(".bashrc")).unwrap(), original);
        assert!(std::path::Path::new(&backup).is_file());
    }

    #[test]
    fn macos_local_native_bash_and_zsh_parse_and_load_only_temp_rc_before_and_after_removal() {
        for (shell, profile) in [("/bin/bash", "bashrc"), ("/bin/zsh", "zshrc")] {
            let shell = if std::path::Path::new(shell).is_file() { shell.to_string() }
                else { format!("/usr/bin/{}", std::path::Path::new(shell).file_name().unwrap().to_str().unwrap()) };
            let home = TempHome::new();
            let original = "# temp only\nalias cc0='claude'\nalias ll='ls -alF'\n";
            home.write(profile, original);
            home.write("bash_profile", ". \"$HOME/.bashrc\"\n");
            let load_alias = |name: &str| {
                let mut cmd = Command::new(&shell);
                if profile == "zshrc" { cmd.arg("-dfc"); }
                else { cmd.args(["--noprofile", "--norc", "-c"]); }
                cmd.arg(format!(". \"$HOME/.{profile}\"; alias {name}"))
                    .env("HOME", &home.0).env_remove("ZDOTDIR").env_remove("BASH_ENV").env_remove("ENV")
                    .stdin(Stdio::null()).stderr(Stdio::null()).output().unwrap()
            };
            let before = load_alias("cc0");
            assert!(before.status.success());
            assert_eq!(parse_probe(std::str::from_utf8(&before.stdout).unwrap(), "cc0", "local").unwrap().as_deref(), Some("claude"));
            let snapshot = run_fake_worker(&home, &shell, request(&home, "snapshot", json!({}))).snapshot("local", "cc0").unwrap();
            let candidate = source_candidate(&snapshot, "cc0", std::str::from_utf8(&before.stdout).unwrap(), "local").unwrap();
            let prepared = run_fake_worker(&home, &shell, request(&home, "prepare", json!({
                "profile":profile,"expected_sha256":digest(original),"candidate":&candidate.replacement
            })));
            assert!(prepared.ok);
            let backup = prepared.backup_path.unwrap();
            let committed = run_fake_worker(&home, &shell, request(&home, "commit", json!({
                "profile":profile,"expected_sha256":digest(original),"candidate":&candidate.replacement,
                "backup_path":backup,"candidate_sha256":digest(&candidate.replacement)
            })));
            assert!(committed.ok, "{committed:?}");
            assert!(!load_alias("cc0").status.success());
            assert!(load_alias("ll").status.success());
            assert_eq!(fs::read_to_string(backup).unwrap(), original);
        }
    }

    #[tokio::test]
    async fn reference_lookup_scopes_bare_bots_kind_and_supervisor_roles_to_their_host() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bare = crate::testing::claude_bot(&app, &env.project_id, "bare-default").await;
        assert_eq!(first_reference(&app, "local", "cc0").await.unwrap(), None, "bare Claude Bot shares credentials but does not explicitly reference cc0");

        let other_kind = crate::testing::claude_bot(&app, &env.project_id, "codex-reference").await;
        sqlx::query("UPDATE bots SET kind='codex', identity='cc0' WHERE id=?").bind(&other_kind.id).execute(&app.db).await.unwrap();
        assert_eq!(first_reference(&app, "local", "cc0").await.unwrap(), None, "non-Claude identity names do not refer to ccN");

        let remote_host = "m4p";
        let remote_project = crate::db::ulid();
        sqlx::query("INSERT INTO projects (id,path,label,host,created_at) VALUES (?, '/tmp/ref-other-host', 'remote', ?, ?)")
            .bind(&remote_project).bind(remote_host).bind(crate::db::now()).execute(&app.db).await.unwrap();
        let remote_bot = crate::testing::claude_bot(&app, &remote_project, "remote-cc0").await;
        sqlx::query("UPDATE bots SET identity='cc0' WHERE id=?").bind(&remote_bot.id).execute(&app.db).await.unwrap();
        assert_eq!(first_reference(&app, "local", "cc0").await.unwrap(), None, "a matching Bot on another host does not block local deletion");
        assert_eq!(first_reference(&app, remote_host, "cc0").await.unwrap(), Some(IdentityRef::bot(remote_bot.id.clone())));

        sqlx::query("UPDATE bots SET identity=NULL WHERE id=?").bind(&remote_bot.id).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO supervisors (id, project_id, identity, created_at, updated_at) VALUES ('remote-patrol', ?, 'cc0', ?, ?)")
            .bind(&remote_project).bind(crate::db::now()).bind(crate::db::now()).execute(&app.db).await.unwrap();
        assert_eq!(first_reference(&app, "local", "cc0").await.unwrap(), None, "supervisor.project_id prevents a remote patrol identity from blocking local removal");
        assert_eq!(first_reference(&app, remote_host, "cc0").await.unwrap(), Some(IdentityRef::supervisor("remote-patrol".into())));
        sqlx::query("UPDATE supervisors SET identity='cc1' WHERE id='remote-patrol'").execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO supervisor_roles (role, bot_id, identity, created_at, updated_at) VALUES ('responder', ?, 'cc0', ?, ?)")
            .bind(&remote_bot.id).bind(crate::db::now()).bind(crate::db::now()).execute(&app.db).await.unwrap();
        assert_eq!(first_reference(&app, "local", "cc0").await.unwrap(), None, "role.bot_id derives the host from the linked Bot's project");
        assert_eq!(first_reference(&app, remote_host, "cc0").await.unwrap(), Some(IdentityRef::role("responder".into())));

        let _ = bare;
    }

    #[tokio::test]
    async fn patrol_supervisor_and_runtime_references_report_their_own_ids() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let patrol = crate::testing::claude_bot(&app, &env.project_id, "patrol-agent").await;
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        crate::supervisor::store::set_env(&app.db, &patrol.id, &env.project_id, "/tmp").await.unwrap();
        sqlx::query("UPDATE supervisors SET identity='cc0' WHERE id='AGM'").execute(&app.db).await.unwrap();
        let reference = first_reference(&app, "local", "cc0").await.unwrap().unwrap();
        assert_eq!(reference.ref_type, "supervisor", "patrol is persisted on supervisors, not supervisor_roles");
        assert_eq!(reference.id, "AGM");

        sqlx::query("UPDATE supervisors SET identity='cc1' WHERE id='AGM'").execute(&app.db).await.unwrap();
        let run_id = crate::testing::fake_run(&app, &patrol.id).await;
        sqlx::query("UPDATE runs SET runtime_identity='cc0' WHERE id=?").bind(&run_id).execute(&app.db).await.unwrap();
        let reference = first_reference(&app, "local", "cc0").await.unwrap().unwrap();
        assert_eq!(reference.ref_type, "runtime");
        assert_eq!(reference.id, run_id, "runtime references report the run id, not its Bot id");
    }

    #[test]
    fn macos_local_unsafe_temp_home_sources_symlink_hardlink_and_writable_files_are_rejected() {
        fn snapshot_for(home: &TempHome) -> Snapshot {
            let reply = run_fake_worker(home, "/bin/bash", request(home, "snapshot", json!({})));
            reply.snapshot("local", "cc0").unwrap()
        }
        fn sourced(home: &TempHome) -> Result<Candidate, LcError> {
            let snapshot = snapshot_for(home);
            source_candidate(&snapshot, "cc0", "alias cc0='claude'\n", "local")
        }
        for unsafe_kind in ["symlink", "hardlink", "writable"] {
            let home = TempHome::new();
            home.write("bash_profile", ". \"$HOME/.bashrc\"\n");
            match unsafe_kind {
                "symlink" => {
                    fs::write(home.0.join(".bashrc-copy"), "alias cc0='claude'\n").unwrap();
                    std::os::unix::fs::symlink(home.0.join(".bashrc-copy"), home.0.join(".bashrc")).unwrap();
                }
                "hardlink" => {
                    home.write("bashrc-copy", "alias cc0='claude'\n");
                    fs::hard_link(home.0.join(".bashrc-copy"), home.0.join(".bashrc")).unwrap();
                }
                "writable" => {
                    home.write("bashrc", "alias cc0='claude'\n");
                    fs::set_permissions(home.0.join(".bashrc"), fs::Permissions::from_mode(0o666)).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(sourced(&home).is_err(), "{unsafe_kind} source must fail closed");
        }
    }

    #[test]
    fn macos_local_zdotdir_mismatch_is_rejected_before_backup_or_rc_write() {
        let home = TempHome::new();
        let other_home = TempHome::new();
        let shell = fake_zsh(&home, false);
        let original = "alias cc0='claude'\n";
        home.write("zshrc", original);
        let reply = run_fake_worker_with_zdot(&home, shell.to_str().unwrap(), Some(&other_home.0), request(&home, "prepare", json!({
            "profile":"zshrc","expected_sha256":digest(original),"candidate":"# removed\n"
        })));
        assert!(!reply.ok);
        assert_eq!(reply.reason.as_deref(), Some("unsupported_zdotdir"));
        assert_eq!(fs::read_to_string(home.0.join(".zshrc")).unwrap(), original);
        assert!(!home.0.join(".config/agents-manager/shell-alias-backups").exists());
    }

    #[test]
    fn macos_local_syntax_failure_keeps_confirmed_backup_and_hides_shell_stderr() {
        let home = TempHome::new();
        let shell = fake_zsh(&home, true);
        let original = "alias cc0='claude'\n";
        home.write("zshrc", original);
        let prepared = run_fake_worker(&home, shell.to_str().unwrap(), request(&home, "prepare", json!({
            "profile":"zshrc","expected_sha256":digest(original),"candidate":"# candidate\n"
        })));
        assert!(prepared.ok);
        let backup = prepared.backup_path.unwrap();
        let reply = run_fake_worker(&home, shell.to_str().unwrap(), request(&home, "commit", json!({
            "profile":"zshrc","expected_sha256":digest(original),"candidate":"# candidate\n",
            "backup_path":backup,"candidate_sha256":digest("# candidate\n")
        })));
        assert!(!reply.ok);
        assert_eq!(reply.reason.as_deref(), Some("syntax_check_failed"));
        assert_eq!(reply.backup_path.as_deref(), Some(backup.as_str()));
        assert_eq!(fs::read_to_string(home.0.join(".zshrc")).unwrap(), original);
        assert_eq!(fs::metadata(backup).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(!format!("{reply:?}").contains("PRIVATE_SYNTAX_ERROR"));
    }

    #[test]
    fn macos_local_local_bash_file_worker_preserves_common_rc_lines_and_removes_only_target_bytes() {
        let home = TempHome::new();
        let body = "CLAUDE_CONFIG_DIR=$HOME/.claude-cc0 claude --dangerously-skip-permissions";
        let original = format!("# temp bash profile\nexport PATH=\"$PATH:$HOME/bin\"\nif [ -f \"$HOME/.bashrc\" ]; then\n  . \"$HOME/.bashrc\"\nfi\n");
        let rc = format!("# keep bytes\nexport PATH=\"$PATH:$HOME/tools\" # keep PATH comment\nexport PS1='\\u@\\h:\\w\\$ '\nalias ll='ls -alF' # keep alias comment\nalias gst=\"git status\" # keep double quote alias\nalias cc0='{body}'\n");
        home.write("bash_profile", &original);
        home.write("bashrc", &rc);
        // The live-shell probe has no source-file metadata. A fake result mirrors the effective
        // alias line from this temp-only fixture; the helper still runs real bash -n on its temp file.
        let active = rc.lines().find(|line| line.starts_with("alias cc0=")).unwrap().to_string() + "\n";
        assert_eq!(parse_probe(&active, "cc0", "local").unwrap().as_deref(), Some(body));
        let snapshot = run_fake_worker(&home, "/bin/bash", request(&home, "snapshot", json!({}))).snapshot("local", "cc0").unwrap();
        let candidate = source_candidate(&snapshot, "cc0", &active, "local").unwrap();
        let prepared = run_fake_worker(&home, "/bin/bash", request(&home, "prepare", json!({
            "profile":candidate.profile,"expected_sha256":digest(&candidate.original),"candidate":&candidate.replacement
        })));
        assert!(prepared.ok);
        let backup = prepared.backup_path.unwrap();
        let committed = run_fake_worker(&home, "/bin/bash", request(&home, "commit", json!({
            "profile":candidate.profile,"expected_sha256":digest(&candidate.original),"candidate":&candidate.replacement,
            "backup_path":backup,"candidate_sha256":digest(&candidate.replacement)
        })));
        assert!(committed.ok, "{committed:?}");
        assert_eq!(fs::read_to_string(home.0.join(".bashrc")).unwrap(), rc.replace(&format!("alias cc0='{body}'\n"), ""));
        assert_eq!(fs::read_to_string(backup).unwrap(), rc);
    }

    #[tokio::test]
    async fn shell_delete_handler_is_user_only_and_remote_success_returns_retained_backup() {
        let (remote, _) = fake_remote(None, false).await;
        let denied = call_delete(remote.env.app.clone(), remote.host.clone(), RequestPrincipal::Bot("bot".into())).await.unwrap_err();
        let (status, body) = error_body(denied).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["reason"], "user_only");
        assert_eq!(remote.worker_calls.load(std::sync::atomic::Ordering::SeqCst), 0, "non-user request must not touch host files");

        let original = fs::read_to_string(remote.home.0.join(".zshrc")).unwrap();
        let response = call_delete(remote.env.app.clone(), remote.host.clone(), RequestPrincipal::User).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        let backup = value["backup_path"].as_str().unwrap();
        assert_eq!(fs::read_to_string(backup).unwrap(), original);
        assert_eq!(fs::metadata(backup).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(!fs::read_to_string(remote.home.0.join(".zshrc")).unwrap().contains("alias cc0="));
        assert!(fs::read_to_string(remote.home.0.join(".zshrc")).unwrap().contains("alias ll='ls -alF'"));
    }

    #[tokio::test]
    async fn lost_prepare_reply_reconciles_the_known_backup_before_commit() {
        let (remote, _) = fake_remote_with_loss(None, false, true).await;
        let original = fs::read_to_string(remote.home.0.join(".zshrc")).unwrap();
        let response = call_delete(remote.env.app.clone(), remote.host.clone(), RequestPrincipal::User).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(fs::read_to_string(body["backup_path"].as_str().unwrap()).unwrap(), original);
        assert_eq!(remote.worker_calls.load(std::sync::atomic::Ordering::SeqCst), 4);
        assert!(!fs::read_to_string(remote.home.0.join(".zshrc")).unwrap().contains("alias cc0="));
    }

    #[tokio::test]
    async fn failed_commit_and_failed_restore_report_restore_conflict_and_keep_external_edit() {
        let (remote, _) = fake_remote_with_faults(None, false, false, true).await;
        let error = call_delete(remote.env.app.clone(), remote.host.clone(), RequestPrincipal::User).await.unwrap_err();
        let (_, body) = error_body(error).await;
        assert_eq!(body["reason"], "alias_restore_conflict");
        assert_eq!(body["restored"], false);
        assert!(std::path::Path::new(body["backup_path"].as_str().unwrap()).is_file());
        assert_eq!(fs::read_to_string(remote.home.0.join(".zshrc")).unwrap(), "# external edit must survive\n");
    }

    #[tokio::test]
    async fn a_preexisting_exact_host_reference_blocks_before_any_remote_file_io() {
        let (remote, _) = fake_remote(None, false).await;
        let bot = add_remote_claude_ref(&remote, "uses-cc0").await;
        let original = fs::read_to_string(remote.home.0.join(".zshrc")).unwrap();
        let error = call_delete(remote.env.app.clone(), remote.host.clone(), RequestPrincipal::User).await.unwrap_err();
        let (status, body) = error_body(error).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["reason"], "identity_in_use");
        assert_eq!(body["refs"][0]["type"], "bot");
        assert_eq!(body["refs"][0]["id"], bot.id);
        assert_eq!(remote.worker_calls.load(std::sync::atomic::Ordering::SeqCst), 0, "pre-reference check must precede worker writes and snapshots");
        assert_eq!(fs::read_to_string(remote.home.0.join(".zshrc")).unwrap(), original);
    }

    #[tokio::test]
    async fn a_lost_commit_reply_attempts_conditional_restore_and_returns_the_confirmed_backup() {
        let (remote, _) = fake_remote(None, true).await;
        let original = fs::read_to_string(remote.home.0.join(".zshrc")).unwrap();
        let error = call_delete(remote.env.app.clone(), remote.host.clone(), RequestPrincipal::User).await.unwrap_err();
        let (status, body) = error_body(error).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["reason"], "alias_commit_outcome_unknown");
        assert_eq!(body["restored"], true);
        let backup = body["backup_path"].as_str().unwrap();
        assert_eq!(fs::read_to_string(backup).unwrap(), original);
        assert_eq!(fs::read_to_string(remote.home.0.join(".zshrc")).unwrap(), original);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reference_added_after_remote_commit_causes_conditional_restore_with_reference() {
        let (remote, pause) = fake_remote(Some(false), false).await;
        let mut pause = pause.unwrap();
        let task = tokio::spawn(call_delete(remote.env.app.clone(), remote.host.clone(), RequestPrincipal::User));
        pause.wait().await;
        let bot = add_remote_claude_ref(&remote, "appeared-after-commit").await;
        pause.release();
        let error = task.await.unwrap().unwrap_err();
        let (status, body) = error_body(error).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["reason"], "alias_new_reference");
        assert_eq!(body["refs"][0]["id"], bot.id);
        assert_eq!(body["restored"], true);
        let backup = body["backup_path"].as_str().unwrap();
        let original = format!("# keep remote\nalias cc0='CLAUDE_CONFIG_DIR=$HOME/.claude-cc0 claude --dangerously-skip-permissions'\nalias ll='ls -alF'\n");
        assert_eq!(fs::read_to_string(remote.home.0.join(".zshrc")).unwrap(), original);
        assert_eq!(fs::read_to_string(backup).unwrap(), original);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_second_same_host_delete_waits_for_the_first_to_finish() {
        use std::sync::atomic::Ordering;
        let (remote, pause) = fake_remote(Some(true), false).await;
        let mut pause = pause.unwrap();
        let first = tokio::spawn(call_delete(remote.env.app.clone(), remote.host.clone(), RequestPrincipal::User));
        pause.wait().await;
        assert_eq!(remote.worker_calls.load(Ordering::SeqCst), 1, "first handler has taken its snapshot and is paused during pre-probe");

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let app = remote.env.app.clone();
        let host = remote.host.clone();
        let mut second = tokio::spawn(async move {
            let _ = started_tx.send(());
            call_delete(app, host, RequestPrincipal::User).await
        });
        started_rx.await.unwrap();
        assert!(tokio::time::timeout(std::time::Duration::from_millis(100), &mut second).await.is_err(), "same-host request must remain behind the first request's host lock");
        assert_eq!(remote.worker_calls.load(Ordering::SeqCst), 1, "the waiting request performed no helper IO");

        pause.release();
        let response = first.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let second = second.await.unwrap().unwrap_err();
        let (_, body) = error_body(second).await;
        assert_eq!(body["reason"], "alias_source_unknown", "after acquiring the lock, the second request sees the first deletion in cache");
        assert_eq!(remote.worker_calls.load(Ordering::SeqCst), 3, "only first snapshot, prepare, and commit reached the fake host worker");
    }
}
