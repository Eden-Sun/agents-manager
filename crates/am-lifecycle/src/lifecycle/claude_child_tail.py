# Invoked through sh/ssh. Keep all reads relative to pinned directory fds;
# checking a path and then calling tail would reopen it after a possible swap.
import os
import stat
import sys


def open_dir(path):
    if not os.path.isabs(path) or '..' in path.split('/'):
        raise ValueError('invalid root')
    fd = os.open('/', os.O_RDONLY | os.O_DIRECTORY)
    try:
        for part in path.split('/'):
            if part and part != '.':
                child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
                os.close(fd)
                fd = child
        return fd
    except BaseException:
        os.close(fd)
        raise


def read_file(cwd_fd, filename, path, limit):
    fd = None
    try:
        fd = os.open(filename, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=cwd_fd)
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            return None
        os.lseek(fd, max(0, info.st_size - limit), os.SEEK_SET)
        body = bytearray()
        while len(body) < limit:
            chunk = os.read(fd, min(65536, limit - len(body)))
            if not chunk:
                break
            body.extend(chunk)
        return path, body
    except OSError:
        return None
    finally:
        if fd is not None:
            os.close(fd)


def read_recorded(root, recorded, sid, limit):
    root_path = root.rstrip('/') or '/'
    prefix = root_path.rstrip('/') + '/projects/' if root_path != '/' else '/projects/'
    if not recorded.startswith(prefix):
        return None
    parts = recorded[len(prefix):].split('/')
    if len(parts) != 2 or parts[0] in ('', '.', '..') or parts[1] != sid + '.jsonl':
        return None

    root_fd = open_dir(root)
    try:
        projects = os.open('projects', os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=root_fd)
        try:
            cwd = os.open(parts[0], os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=projects)
            try:
                return read_file(cwd, parts[1], recorded, limit)
            finally:
                os.close(cwd)
        finally:
            os.close(projects)
    finally:
        os.close(root_fd)


def read_root(root, sid, limit, seen_paths, matches):
    root_fd = open_dir(root)
    try:
        projects = os.open('projects', os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=root_fd)
        try:
            for name in sorted(os.listdir(projects)):
                if name.startswith('.'):
                    continue
                cwd = None
                try:
                    cwd = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=projects)
                    path = os.path.join(root, 'projects', name, sid + '.jsonl')
                    result = read_file(cwd, sid + '.jsonl', path, limit)
                    if result is not None and path not in seen_paths:
                        seen_paths.add(path)
                        matches.append(result)
                        if len(matches) > 1:
                            return True
                except OSError:
                    continue
                finally:
                    if cwd is not None:
                        os.close(cwd)
        finally:
            os.close(projects)
    finally:
        os.close(root_fd)
    return False


sid, limit, marker, recorded = sys.argv[1:5]
limit = int(limit)
roots = sys.argv[5:]
result = None

if recorded:
    for root in roots:
        try:
            result = read_recorded(root, recorded, sid, limit)
        except (OSError, ValueError):
            continue
        if result is not None:
            break
else:
    matches = []
    seen_paths = set()
    ambiguous = False
    for root in roots:
        try:
            if read_root(root, sid, limit, seen_paths, matches):
                ambiguous = True
                break
        except (OSError, ValueError):
            continue
    if not ambiguous and len(matches) == 1:
        result = matches[0]

if result is not None:
    path, body = result
    sys.stdout.buffer.write((marker + '\n' + path + '\n').encode() + body)
