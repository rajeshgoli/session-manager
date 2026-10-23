#!/usr/bin/env python3
"""Read a small existing external-volume file from the real queue launch context.

Submit through sm queue run; a direct-shell pass does not verify queue access.
No file on the volume is changed. Each read has its own hard timeout.
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


def read_case(case, timeout):
    kind, path = case
    command = [sys.executable, __file__, '--reader', kind, path]
    started = time.monotonic()
    process = subprocess.Popen(command, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=timeout)
        result = {'reader': kind, 'path': path, 'exit': process.returncode,
                  'bytes': len(stdout), 'sha256': hashlib.sha256(stdout).hexdigest()}
        if stderr:
            result['error'] = stderr.decode(errors='replace')[:1000]
    except subprocess.TimeoutExpired:
        # Kill the reader AND its cat descendant; don't leave blocked opens behind.
        os.killpg(process.pid, signal.SIGKILL)
        process.communicate()
        result = {'reader': kind, 'path': path, 'timeout': True}
    result['pid'] = process.pid
    result['seconds'] = round(time.monotonic() - started, 3)
    return result


def reader(kind, path):
    resolved = Path(path).resolve(strict=True)
    if not str(resolved).startswith('/Volumes/'):
        raise ValueError(f'{path} does not resolve to an external /Volumes file')
    if not resolved.is_file() or resolved.stat().st_size > 65536:
        raise ValueError('choose an existing regular reference file of at most 64 KiB')
    if kind == 'cat':
        os.execv('/bin/cat', ['/bin/cat', path])
    sys.stdout.buffer.write(Path(path).read_bytes())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('paths', nargs='+', help='small file paths, e.g. alias and resolved path')
    parser.add_argument('--timeout', type=float, default=8)
    parser.add_argument('--reader', choices=['cat', 'python'], help=argparse.SUPPRESS)
    args = parser.parse_args()
    if not 0 < args.timeout <= 60:
        parser.error('--timeout must be greater than zero and at most 60 seconds')
    if sys.platform != 'darwin':
        print('SKIP: this check requires macOS and an existing external-volume file')
        return 0
    if args.reader:
        reader(args.reader, args.paths[0])
        return 0
    cases = [(kind, path) for path in args.paths for kind in ('cat', 'python')]
    with ThreadPoolExecutor(max_workers=min(4, len(cases))) as pool:
        results = list(pool.map(lambda case: read_case(case, args.timeout), cases))
    print(json.dumps(results, indent=2), flush=True)
    passed = all(result.get('exit') == 0 for result in results)
    # All supplied paths must identify the same reference file.
    passed = passed and len({result.get('sha256') for result in results}) == 1
    if not passed:
        print('FAIL: external-volume reads failed or returned different content. '
              'Check the active sm-server executable in System Settings > Privacy & Security '
              '> Files and Folders / Full Disk Access, and inspect sandboxd/tccd logs. '
              'Both sm-server-blue and sm-server-green launch paths need the intended access. '
              'Do not move the artifact or reset privacy permissions to conceal this failure.',
              file=sys.stderr)
        return 1
    print('PASS: cat and Python read identical content through every supplied path')
    return 0


if __name__ == '__main__':
    sys.exit(main())
