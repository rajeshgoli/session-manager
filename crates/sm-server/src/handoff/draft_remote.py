import sys, json, os, hashlib, subprocess
r = json.load(sys.stdin)
d = r['draft']
p = os.path.expanduser(d['path'])
if r['action'] == 'write':
    folder = os.path.dirname(p)
    root = os.path.dirname(folder)
    os.makedirs(folder, exist_ok=True)
    git = subprocess.run(['git', '-C', root, 'rev-parse', '--git-path', 'info/exclude'], capture_output=True, text=True)
    if git.returncode == 0:
        exclude = git.stdout.strip()
        if not os.path.isabs(exclude): exclude = os.path.join(root, exclude)
        os.makedirs(os.path.dirname(exclude), exist_ok=True)
        previous = open(exclude).read() if os.path.exists(exclude) else ''
        if '.sm/' not in previous.splitlines():
            with open(exclude, 'a') as f: f.write(('\n' if previous and not previous.endswith('\n') else '') + '.sm/\n')
    try:
        with open(p, 'x') as f: f.write(d['text'])
    except FileExistsError: pass
elif os.path.exists(p):
    expected = r.get('expected')
    if expected is None or hashlib.sha256(open(p, 'rb').read()).hexdigest() == expected:
        os.unlink(p)
        try: os.rmdir(os.path.dirname(p))
        except OSError: pass

print(json.dumps(not os.path.exists(p) if r["action"] != "write" else True))
