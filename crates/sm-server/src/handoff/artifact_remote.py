# Operates only on the path supplied by the primary server, on this session's node.
import json, os, subprocess, sys
r = json.load(sys.stdin)
p = os.path.expanduser(r['path'])
def snapshot(s):
    return dict(device=s.st_dev, inode=s.st_ino, size=s.st_size, mtime=str(s.st_mtime_ns))
if r['action'] == 'read':
    try:
        with open(p, 'rb') as f:
            before = snapshot(os.fstat(f.fileno()))
            text = f.read().decode('utf-8')
        print(json.dumps(dict(exists=True, text=text, snapshot=before)))
    except FileNotFoundError:
        print(json.dumps(dict(exists=False)))
else:
    removed, reason = False, None
    try:
        before = r['snapshot']
        tracked = subprocess.run(['git', '-C', os.path.dirname(p), 'ls-files', '--error-unmatch', '--', p], capture_output=True).returncode == 0
        if tracked:
            reason = 'git tracks it'
        elif snapshot(os.stat(p)) != {key: before.get(key) for key in ('device', 'inode', 'size', 'mtime')}:
            reason = 'device, inode, size or mtime changed since the read'
        else:
            os.unlink(p)
            removed = True
    except FileNotFoundError:
        removed = True
    except OSError as e:
        reason = str(e)
    print(json.dumps(dict(removed=removed, reason=reason)))
