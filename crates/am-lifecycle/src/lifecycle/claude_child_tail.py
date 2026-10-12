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


def read_root(root, sid, limit):
    root_fd = open_dir(root)
    try:
        projects = os.open('projects', os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=root_fd)
        try:
            for name in sorted(os.listdir(projects)):
                if name.startswith('.'):
                    continue
                cwd = None
                fd = None
                try:
                    cwd = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=projects)
                    fd = os.open(sid + '.jsonl', os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=cwd)
                    info = os.fstat(fd)
                    if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
                        continue
                    os.lseek(fd, max(0, info.st_size - limit), os.SEEK_SET)
                    body = bytearray()
                    while len(body) < limit:
                        chunk = os.read(fd, min(65536, limit - len(body)))
                        if not chunk:
                            break
                        body.extend(chunk)
                    path = os.path.join(root, 'projects', name, sid + '.jsonl')
                    return path, body
                except OSError:
                    continue
                finally:
                    if fd is not None:
                        os.close(fd)
                    if cwd is not None:
                        os.close(cwd)
        finally:
            os.close(projects)
    finally:
        os.close(root_fd)


sid, limit, marker = sys.argv[1:4]
for root in sys.argv[4:]:
    try:
        result = read_root(root, sid, int(limit))
    except (OSError, ValueError):
        continue
    if result is not None:
        path, body = result
        sys.stdout.buffer.write((marker + '\n' + path + '\n').encode() + body)
        break
