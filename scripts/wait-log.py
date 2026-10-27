#!/usr/bin/env python3
"""Read-only background log waiter; never starts/stops the producing job.

Examples:
  scripts/wait-log.py build.log --exit-prefix BUILD_RC= --timeout 600
  scripts/wait-log.py task.log --line-prefix DONE --tail 20
Markers mean only that text was observed. Prefer an exit-prefix when the producer
records its exit status. Exit: producer RC, 0 marker, 124 deadline, 2 input/I/O
error. RC records require a newline. Rotation/truncation discard old partial lines
and tail; an undetectable truncate-and-regrow between polls cannot be recovered.
Only regular, non-symlink files are accepted; output and pending lines are bounded.
"""
import argparse
from collections import deque
import math
import os
from pathlib import Path
import re
import stat
import sys
import time

CHUNK = 65536
MAX_LINE = 1024 * 1024
MAX_TAIL_BYTES = 65536


def positive(value):
    number = float(value)
    if not math.isfinite(number) or number <= 0:
        raise argparse.ArgumentTypeError('must be finite and positive')
    return number


def parser():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('log', type=Path)
    mode = p.add_mutually_exclusive_group(required=True)
    mode.add_argument('--contains', action='append', help='literal substring (repeat for OR)')
    mode.add_argument('--line-prefix', action='append', help='complete line prefix (repeat for OR)')
    mode.add_argument('--exit-prefix', help='complete line prefix followed by integer exit status 0..255')
    p.add_argument('--timeout', type=positive, default=60.0)
    p.add_argument('--interval', type=positive, default=0.2)
    p.add_argument('--tail', type=int, default=20, help='last 0..200 lines')
    p.add_argument('--max-output', type=int, default=4000, help='last 1..65536 output characters')
    return p


def wait(args, clock=time.monotonic, sleep=time.sleep, out=sys.stdout, err=sys.stderr):
    deadline = clock() + args.timeout
    identity = None
    position = 0
    partial = b''
    tail = deque(maxlen=args.tail)
    tail_bytes = 0

    def finish(rc, reason):
        content = b'\n'.join(tail)
        if partial:
            content += (b'\n' if content else b'') + partial
        if content:
            print(content.decode('utf-8', 'replace')[-args.max_output:], file=out)
        print(reason, file=err)
        return rc

    while True:
        if clock() >= deadline:
            return finish(124, 'wait-log: deadline reached')
        try:
            # Nonblocking open avoids hanging on FIFOs; fstat also rejects devices.
            fd = os.open(args.log, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW)
            with os.fdopen(fd, 'rb') as stream:
                info = os.fstat(stream.fileno())
                if not stat.S_ISREG(info.st_mode):
                    return finish(2, 'wait-log: log must be a regular file')
                current = (info.st_dev, info.st_ino)
                if identity != current or info.st_size < position:
                    identity, position, partial = current, 0, b''
                    tail.clear()
                    tail_bytes = 0
                stream.seek(position)
                data = stream.read(min(CHUNK, max(0, info.st_size - position)))
                position += len(data)
        except FileNotFoundError:
            data = b''
        except OSError as exc:
            return finish(2, 'wait-log: cannot read log: ' + exc.strerror)

        if data:
            partial += data
            while b'\n' in partial:
                line, partial = partial.split(b'\n', 1)
                if len(line) > MAX_LINE:
                    return finish(2, 'wait-log: line exceeds 1 MiB')
                if args.tail:
                    if len(tail) == tail.maxlen:
                        tail_bytes -= len(tail[0])
                    kept = line[-MAX_TAIL_BYTES:]
                    tail.append(kept)
                    tail_bytes += len(kept)
                    while tail_bytes > MAX_TAIL_BYTES:
                        tail_bytes -= len(tail.popleft())
                text = line.decode('utf-8', 'replace').rstrip('\r')
                rc = None
                if args.exit_prefix and text.startswith(args.exit_prefix):
                    value = text[len(args.exit_prefix):]
                    if not re.fullmatch(r'[0-9]{1,3}', value) or int(value) > 255:
                        return finish(2, 'wait-log: invalid exit status record')
                    rc = int(value)
                elif args.line_prefix and any(text.startswith(x) for x in args.line_prefix):
                    rc = 0
                elif args.contains and any(x in text for x in args.contains):
                    rc = 0
                if rc is not None:
                    # Do not print bytes after the matched record from the same chunk.
                    partial = b''
                    return finish(rc, 'wait-log: matched completion record')
            if len(partial) > MAX_LINE:
                return finish(2, 'wait-log: line exceeds 1 MiB')
            if args.contains and any(x.encode() in partial for x in args.contains):
                return finish(0, 'wait-log: matched literal')
            # Drain a growing file without sleeping between chunks, bounded by deadline.
            continue
        sleep(min(args.interval, max(0, deadline - clock())))


def main(argv=None):
    p = parser()
    args = p.parse_args(argv)
    markers = args.contains or args.line_prefix or [args.exit_prefix]
    if any(not x for x in markers):
        p.error('markers must not be empty')
    if not 0 <= args.tail <= 200 or not 1 <= args.max_output <= MAX_TAIL_BYTES:
        p.error('tail must be 0..200 and max-output 1..65536')
    try:
        return wait(args)
    except KeyboardInterrupt:
        return 130


if __name__ == '__main__':
    sys.exit(main())
