#!/usr/bin/env python3
"""移交 project-transfer 之外的主機狀態：config、UI token、outbox 與 identity 目錄清單。

這支不讀寫 SQLite，也不搬 identity 憑證。install/restore 必須在 daemon.lock 可取得時執行。
"""

import argparse
import fcntl
import hashlib
import json
import os
import re
import shutil
import stat
import sys
import tempfile
import tomllib
from pathlib import Path, PurePosixPath


FORMAT = "agm-host-state/1"
HOME_VARS = {"claude": "CLAUDE_CONFIG_DIR", "codex": "CODEX_HOME", "grok": "GROK_HOME"}
DEFAULT_DIRS = {"claude": ".claude", "codex": ".codex", "grok": ".grok"}


class TransferError(Exception):
    pass


def fail(message):
    raise TransferError(message)


def regular_file(path, label):
    try:
        mode = path.lstat().st_mode
    except OSError as exc:
        fail(f"{label}: {exc}")
    if stat.S_ISLNK(mode) or not stat.S_ISREG(mode):
        fail(f"{label} 必須是一般檔案（不跟隨 symlink）：{path}")


def directory(path, label):
    try:
        mode = path.lstat().st_mode
    except OSError as exc:
        fail(f"{label}: {exc}")
    if stat.S_ISLNK(mode) or not stat.S_ISDIR(mode):
        fail(f"{label} 必須是一般目錄（不跟隨 symlink）：{path}")


def parse_maps(items):
    maps = []
    for item in items or []:
        src, sep, dst = item.partition("=")
        if not sep or not src.startswith("/") or not dst.startswith("/"):
            fail(f"--map 必須是絕對路徑 SRC=DST：{item!r}")
        src, dst = os.path.normpath(src), os.path.normpath(dst)
        if src == dst:
            continue
        maps.append((src, dst))
    return sorted(maps, key=lambda pair: len(pair[0]), reverse=True)


def rewrite_paths(text, maps):
    reports = []
    for src, dst in maps:
        # 只換完整路徑前綴，不把 /Users/m4p-old 或一般單字當成路徑。
        pattern = re.compile(r"(?<![A-Za-z0-9_./-])" + re.escape(src) + r"(?=$|[/\\\"'\s,:;\]\}\)])")
        text, count = pattern.subn(dst, text)
        if count:
            reports.append({"from": src, "to": dst, "occurrences": count})
    return text, reports


def expand_home(value, home):
    raw = str(value)
    raw = raw.replace("${HOME}", str(home)).replace("$HOME", str(home))
    if raw == "~":
        raw = str(home)
    elif raw.startswith("~/"):
        raw = str(home / raw[2:])
    return os.path.normpath(raw) if raw.startswith("/") else raw


def identity_inventory(config, home):
    """只列 identity 使用的設定目錄路徑與是否存在，絕不讀目錄內容。"""
    rows = {}

    def add(identity, kind, raw_path, source):
        path = expand_home(raw_path, home)
        key = path
        if key not in rows:
            rows[key] = {"identity": identity, "kind": kind, "path": path,
                         "exists": Path(path).is_dir(), "source": source}
        elif rows[key]["identity"] is None and identity:
            rows[key]["identity"] = identity
            rows[key]["kind"] = kind

    for kind, rel in DEFAULT_DIRS.items():
        add(None, kind, str(home / rel), "default")

    for identity in config.get("identities") or []:
        if not isinstance(identity, dict):
            continue
        name = identity.get("name")
        kind = identity.get("kind")
        if kind not in HOME_VARS:
            continue
        env = identity.get("env") or {}
        raw = env.get(HOME_VARS[kind]) if isinstance(env, dict) else None
        add(name, kind, raw or str(home / DEFAULT_DIRS[kind]), "config")

    # shell alias identities (ccN) 可能不在 config.toml；只列家目錄下同類目錄名稱。
    for kind in HOME_VARS:
        for path in sorted(home.glob(f".{kind}*")):
            if path.is_dir() and not path.is_symlink():
                add(None, kind, str(path), "home-directory")
    return {"format": FORMAT, "directories": sorted(rows.values(), key=lambda row: row["path"])}


def copy_tree_no_links(source, destination):
    files = []
    if not source.exists() and not source.is_symlink():
        return files
    directory(source, "source outbox")
    destination.mkdir(mode=0o700)
    for current, dirs, names in os.walk(source, followlinks=False):
        current_path = Path(current)
        relative_dir = current_path.relative_to(source)
        out_dir = destination / relative_dir
        out_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
        for name in dirs:
            p = current_path / name
            if p.is_symlink() or not p.is_dir():
                fail(f"outbox 含 symlink 或非目錄項目：{p}")
        for name in names:
            src = current_path / name
            regular_file(src, "outbox 項目")
            rel = (relative_dir / name).as_posix()
            dst = destination / rel
            data = src.read_bytes()
            write_new(dst, data, 0o600)
            files.append({"path": rel, "sha256": digest(data), "size": len(data)})
    return sorted(files, key=lambda row: row["path"])


def digest(data):
    return hashlib.sha256(data).hexdigest()


def write_new(path, data, mode):
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    fd, temp = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        os.fchmod(fd, mode)
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temp, path)
        os.chmod(path, mode)
    finally:
        try:
            os.unlink(temp)
        except FileNotFoundError:
            pass


def write_json(path, value):
    write_new(path, (json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode(), 0o600)


def read_json(path, label):
    regular_file(path, label)
    try:
        return json.loads(path.read_text())
    except (OSError, ValueError) as exc:
        fail(f"{label} 讀取失敗：{exc}")


def load_bundle(path):
    directory(path, "bundle")
    manifest = read_json(path / "manifest.json", "bundle manifest")
    if manifest.get("format") != FORMAT:
        fail("不認得的 host-state bundle 格式")
    regular_file(path / "config.toml", "bundle config.toml")
    regular_file(path / "ui-token", "bundle ui-token")
    read_json(path / "identity-directories.json", "identity directory 清單")
    try:
        config = tomllib.loads((path / "config.toml").read_text())
    except (OSError, tomllib.TOMLDecodeError) as exc:
        fail(f"bundle config.toml 無法解析：{exc}")
    outbox = path / "outbox"
    if outbox.exists():
        directory(outbox, "bundle outbox")
    for row in manifest.get("outbox_files") or []:
        safe_relative(row.get("path"))
        regular_file(outbox / row["path"], "bundle outbox 項目")
    return manifest, config


def snapshot(args):
    source = Path(args.source_data)
    home = Path(args.source_home).resolve()
    out = Path(args.out).absolute()
    directory(source, "source data")
    directory(home, "source home")
    for name in ("config.toml", "ui-token"):
        regular_file(source / name, f"source {name}")
    if out.exists() or out.is_symlink():
        fail(f"bundle 目標已存在：{out}")
    if not out.parent.is_dir():
        fail(f"bundle 父目錄不存在：{out.parent}")

    maps = parse_maps(args.map)
    temp = Path(tempfile.mkdtemp(prefix=f".{out.name}.", dir=out.parent))
    os.chmod(temp, 0o700)
    try:
        original = (source / "config.toml").read_text()
        try:
            original_config = tomllib.loads(original)
        except tomllib.TOMLDecodeError as exc:
            fail(f"來源 config.toml 無法解析：{exc}")
        rewritten, rewrite_report = rewrite_paths(original, maps)
        try:
            config = tomllib.loads(rewritten)
        except tomllib.TOMLDecodeError as exc:
            fail(f"路徑改寫後 config.toml 無法解析：{exc}")
        write_new(temp / "config.toml", rewritten.encode(), 0o600)
        write_new(temp / "ui-token", (source / "ui-token").read_bytes(), 0o600)
        outbox_files = copy_tree_no_links(source / "outbox", temp / "outbox")
        identities = identity_inventory(original_config, home)
        write_json(temp / "identity-directories.json", identities)
        write_json(temp / "path-rewrites.json", {"format": FORMAT, "rewrites": rewrite_report})
        manifest = {
            "format": FORMAT,
            "outbox_present": (source / "outbox").is_dir(),
            "outbox_files": outbox_files,
            "config_sha256": digest(rewritten.encode()),
            "ui_token_sha256": digest((source / "ui-token").read_bytes()),
            "identity_directories": len(identities["directories"]),
            "path_rewrites": rewrite_report,
        }
        write_json(temp / "manifest.json", manifest)
        os.replace(temp, out)
    except BaseException:
        shutil.rmtree(temp, ignore_errors=True)
        raise
    print(json.dumps({"bundle": str(out), "outbox_files": len(outbox_files),
                      "identity_directories": manifest["identity_directories"],
                      "path_rewrites": rewrite_report}, ensure_ascii=False))


def safe_relative(raw):
    if not isinstance(raw, str):
        fail("outbox manifest path 必須是字串")
    p = PurePosixPath(raw)
    if p.is_absolute() or not p.parts or any(part in ("", ".", "..") for part in p.parts):
        fail(f"不安全的 outbox 相對路徑：{raw!r}")
    return p


def check_outbox_target(target, files):
    root = target / "outbox"
    if root.exists() or root.is_symlink():
        directory(root, "target outbox")
    plan = []
    for row in files:
        rel = safe_relative(row["path"])
        path = root / Path(*rel.parts)
        current = root
        for part in rel.parts[:-1]:
            current = current / part
            if current.exists() or current.is_symlink():
                directory(current, "target outbox 子目錄")
        if path.exists() or path.is_symlink():
            regular_file(path, "target outbox 項目")
            data = path.read_bytes()
            if digest(data) != row["sha256"]:
                fail(f"target outbox 同名檔內容不同，停止避免覆寫：{rel.as_posix()}")
            plan.append({"path": rel.as_posix(), "action": "already-present"})
        else:
            plan.append({"path": rel.as_posix(), "action": "copy"})
    return plan


def config_blocks(text):
    """把 TOML 依 table header 分塊，便於將 project-transfer 寫好的 projects 保留下來。"""
    blocks = []
    current, root = [], None
    header = re.compile(r"^\s*\[\[?([^]\[]+)\]\]?\s*(?:#.*)?$")
    for line in text.splitlines(keepends=True):
        match = header.match(line)
        if match:
            if current:
                blocks.append((root, "".join(current)))
            current = [line]
            root = match.group(1).strip().strip('"').split(".", 1)[0].strip('"')
        else:
            current.append(line)
    if current:
        blocks.append((root, "".join(current)))
    return blocks


def merge_config(source_text, target_text):
    try:
        source = tomllib.loads(source_text)
        target = tomllib.loads(target_text)
    except tomllib.TOMLDecodeError as exc:
        fail(f"config.toml 無法解析：{exc}")
    source_other = [block for root, block in config_blocks(source_text) if root != "projects"]
    target_projects = [block for root, block in config_blocks(target_text) if root == "projects"]
    # project sections 只保留目標現有定義；沒有的留給後續 project-transfer append。
    project_blocks = target_projects
    merged = "\n".join(part.rstrip("\n") for part in source_other + project_blocks).rstrip() + "\n"
    try:
        merged_obj = tomllib.loads(merged)
    except tomllib.TOMLDecodeError as exc:
        fail(f"合併後 config.toml 無法解析：{exc}")
    return merged, source, target, merged_obj


def take_lock(target):
    path = target / "daemon.lock"
    regular_file(path, "target daemon.lock")
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except (BlockingIOError, OSError):
        os.close(fd)
        fail(f"目標 daemon.lock 仍被持有，拒絕移交設定：{path}")
    return fd


def atomic_write(path, data, mode=0o600):
    if path.exists() or path.is_symlink():
        regular_file(path, str(path))
    write_new(path, data, mode)


def prepare_backup(backup, target):
    if backup.exists() or backup.is_symlink():
        directory(backup, "backup 目錄")
        meta_path = backup / "backup.json"
        meta = read_json(meta_path, "backup manifest")
        if meta.get("format") != FORMAT:
            fail(f"backup 目錄格式不符：{backup}")
        return meta
    backup.mkdir(mode=0o700, parents=True)
    meta = {"format": FORMAT, "had_config": (target / "config.toml").exists(),
            "had_ui_token": (target / "ui-token").exists(), "outbox_added": [], "restored": False}
    for name, exists in (("config.toml", meta["had_config"]), ("ui-token", meta["had_ui_token"])):
        if exists:
            regular_file(target / name, f"target {name}")
            write_new(backup / name, (target / name).read_bytes(), 0o600)
    write_json(backup / "backup.json", meta)
    return meta


def remove_added_outbox(target, meta):
    removed, preserved = [], []
    root = target / "outbox"
    for row in meta.get("outbox_added") or []:
        rel = safe_relative(row["path"])
        path = root / Path(*rel.parts)
        if not path.exists() and not path.is_symlink():
            continue
        regular_file(path, "outbox rollback 項目")
        if digest(path.read_bytes()) == row["sha256"]:
            path.unlink()
            removed.append(rel.as_posix())
        else:
            preserved.append(rel.as_posix())
    for parent in sorted((p for p in root.rglob("*") if p.is_dir() and not p.is_symlink()),
                         key=lambda p: len(p.parts), reverse=True) if root.is_dir() else []:
        try:
            parent.rmdir()
        except OSError:
            pass
    return removed, preserved


def restore_locked(target, backup, meta, mark_restored=True):
    errors = []
    for name, key in (("config.toml", "had_config"), ("ui-token", "had_ui_token")):
        path, saved = target / name, backup / name
        if meta.get(key):
            regular_file(saved, f"backup {name}")
            atomic_write(path, saved.read_bytes())
        elif path.exists() or path.is_symlink():
            regular_file(path, f"target {name}")
            path.unlink()
    removed, preserved = remove_added_outbox(target, meta)
    if preserved:
        errors.append("保留切換後已變更的 outbox 檔：" + ", ".join(preserved))
    if mark_restored:
        meta["restored"] = True
    write_json(backup / "backup.json", meta)
    return {"restored": True, "outbox_removed": removed, "warnings": errors}


def install(args):
    bundle = Path(args.bundle)
    target = Path(args.target_data)
    backup = Path(args.backup_dir)
    manifest, source_cfg = load_bundle(bundle)
    directory(target, "target data")
    regular_file(target / "config.toml", "target config.toml")
    fd = take_lock(target)
    try:
        target_text = (target / "config.toml").read_text()
        merged_text, source_obj, _target_obj, merged_obj = merge_config(
            (bundle / "config.toml").read_text(), target_text)
        outbox_plan = check_outbox_target(target, manifest.get("outbox_files") or [])
        report = {"config_sections": sorted(k for k in source_obj if k != "projects"),
                  "project_sections_preserved": len(merged_obj.get("projects") or []),
                  "outbox_files": sum(1 for row in outbox_plan if row["action"] == "copy"),
                  "outbox_already_present": sum(1 for row in outbox_plan if row["action"] == "already-present"),
                  "identity_directories": manifest.get("identity_directories", 0),
                  "ui_token": "copy (value hidden)"}
        if args.dry_run:
            print(json.dumps({"dry_run": True, **report}, ensure_ascii=False))
            return

        meta = prepare_backup(backup, target)
        if meta.get("restored"):
            fail("這份 backup 已 rollback，請為新的移交使用新的 backup 目錄")
        copied_now = []
        try:
            atomic_write(target / "config.toml", merged_text.encode())
            atomic_write(target / "ui-token", (bundle / "ui-token").read_bytes())
            outbox_root = target / "outbox"
            if manifest.get("outbox_present") and not outbox_root.exists():
                outbox_root.mkdir(mode=0o700)
            for row in manifest.get("outbox_files") or []:
                if row["path"] not in {item["path"] for item in meta.get("outbox_added") or []}:
                    rel = safe_relative(row["path"])
                    dst = outbox_root / Path(*rel.parts)
                    if dst.exists():
                        continue
                    write_new(dst, (bundle / "outbox" / Path(*rel.parts)).read_bytes(), 0o600)
                    item = {"path": rel.as_posix(), "sha256": row["sha256"]}
                    meta.setdefault("outbox_added", []).append(item)
                    copied_now.append(item)
                    write_json(backup / "backup.json", meta)
            if tomllib.loads((target / "config.toml").read_text()) != merged_obj:
                fail("套用後 config.toml 重讀結果不符")
            meta["installed"] = True
            write_json(backup / "backup.json", meta)
        except BaseException:
            for item in copied_now:
                rel = safe_relative(item["path"])
                path = target / "outbox" / Path(*rel.parts)
                if path.is_file() and not path.is_symlink() and digest(path.read_bytes()) == item["sha256"]:
                    path.unlink()
            # 若是既有 backup，恢復目標原始 config/token；否則使用剛建立的 backup。
            restore_locked(target, backup, meta, mark_restored=False)
            meta["outbox_added"] = []
            meta["installed"] = False
            meta["restored"] = False
            write_json(backup / "backup.json", meta)
            raise
        print(json.dumps({"installed": True, **report}, ensure_ascii=False))
    finally:
        os.close(fd)


def verify(args):
    bundle, target = Path(args.bundle), Path(args.target_data)
    manifest, source_cfg = load_bundle(bundle)
    directory(target, "target data")
    regular_file(target / "config.toml", "target config.toml")
    regular_file(target / "ui-token", "target ui-token")
    try:
        target_cfg = tomllib.loads((target / "config.toml").read_text())
    except (OSError, tomllib.TOMLDecodeError) as exc:
        fail(f"target config.toml 無法解析：{exc}")
    source_other = {k: v for k, v in source_cfg.items() if k != "projects"}
    target_other = {k: v for k, v in target_cfg.items() if k != "projects"}
    problems = []
    if source_other != target_other:
        problems.append("source 非 project 設定與 target config.toml 不一致")
    if digest((target / "ui-token").read_bytes()) != manifest.get("ui_token_sha256"):
        problems.append("target ui-token 與 bundle 不一致")
    outbox_plan = check_outbox_target(target, manifest.get("outbox_files") or [])
    missing = [row["path"] for row in outbox_plan if row["action"] != "already-present"]
    if missing:
        problems.append(f"target outbox 缺 {len(missing)} 個 bundle 檔案")
    read_json(bundle / "identity-directories.json", "identity directory 清單")
    report = {"ok": not problems, "config_sections": len(source_other),
              "outbox_files": len(manifest.get("outbox_files") or []),
              "identity_directories": manifest.get("identity_directories", 0), "problems": problems}
    print(json.dumps(report, ensure_ascii=False))
    if problems:
        fail("host-state 驗證失敗")


def restore(args):
    target, backup = Path(args.target_data), Path(args.backup_dir)
    directory(target, "target data")
    if not backup.exists():
        print(json.dumps({"restored": False, "reason": "no backup"}))
        return
    directory(backup, "backup 目錄")
    meta = read_json(backup / "backup.json", "backup manifest")
    if meta.get("format") != FORMAT:
        fail("不認得的 host-state backup 格式")
    if meta.get("restored"):
        print(json.dumps({"restored": True, "already_restored": True}))
        return
    fd = take_lock(target)
    try:
        report = restore_locked(target, backup, meta)
    finally:
        os.close(fd)
    print(json.dumps(report, ensure_ascii=False))


def add_maps(parser):
    parser.add_argument("--map", action="append", default=[], metavar="SRC=DST")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subs = parser.add_subparsers(dest="command", required=True)
    p = subs.add_parser("snapshot", help="secure snapshot of config/token/outbox and identity paths")
    p.add_argument("--source-data", required=True)
    p.add_argument("--source-home", required=True)
    p.add_argument("--out", required=True)
    add_maps(p)
    p.set_defaults(run=snapshot)
    p = subs.add_parser("install", help="install into a stopped target without touching SQLite")
    p.add_argument("--bundle", required=True)
    p.add_argument("--target-data", required=True)
    p.add_argument("--backup-dir", required=True)
    p.add_argument("--dry-run", action="store_true")
    p.set_defaults(run=install)
    p = subs.add_parser("verify", help="verify installed auxiliary state")
    p.add_argument("--bundle", required=True)
    p.add_argument("--target-data", required=True)
    p.set_defaults(run=verify)
    p = subs.add_parser("restore", help="restore config/token and remove unchanged imported outbox files")
    p.add_argument("--backup-dir", required=True)
    p.add_argument("--target-data", required=True)
    p.set_defaults(run=restore)
    args = parser.parse_args()
    try:
        args.run(args)
    except (TransferError, OSError, ValueError) as exc:
        print(f"host-state-transfer: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
