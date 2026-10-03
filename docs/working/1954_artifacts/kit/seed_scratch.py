#!/usr/bin/env python3
"""#1954: seed the scratch sm's sessions.json with the parent (sm-1954) and every agent in agents.json,
each with a fresh session credential (id and credential written to runs-1954/<run>/sm_id and sm_credential for the agent's env).
usage: seed_scratch.py <agents.json> <sessions.json out>"""
import hashlib, json, os, secrets, sys, datetime
agents_path, out = sys.argv[1:]
runs = os.path.dirname(os.path.abspath(agents_path))
now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def record(sid, name, wd, parent, cred):
    return {"id": sid, "name": name, "friendly_name": name, "friendly_name_is_explicit": True,
            "provider": "claude", "status": "running", "working_dir": wd, "parent_session_id": parent,
            "created_at": now, "spawned_at": now, "last_activity": now, "started_by_sm": True,
            "model": "proto", "node": "primary", "is_em": False, "turns_completed": 0,
            "tmux_session": f"l1954-{sid}", "tmux_socket_name": "l1954-scratch",
            "log_file": f"/private/tmp/l1954-sm/logs/{sid}.log", "completion_status": None,
            "session_credential_sha256": hashlib.sha256(cred.encode()).hexdigest()}


sessions = [record("p1954000", "sm-1954", "/Users/rajesh/projects/session-manager", None, secrets.token_hex(16))]
for aid, a in json.load(open(agents_path)).items():
    if not aid.startswith("local-"):
        continue
    cred = secrets.token_hex(24)
    run = aid[len("local-"):]
    os.makedirs(f"{runs}/{run}", exist_ok=True)
    sid = hashlib.sha1(aid.encode()).hexdigest()[:8]  # sm ids are 8 hex characters
    with open(f"{runs}/{run}/sm_credential", "w") as f:
        f.write(cred)
    with open(f"{runs}/{run}/sm_id", "w") as f:
        f.write(sid)
    sessions.append(record(sid, a["name"], a["checkout"], "p1954000", cred))
os.makedirs(os.path.dirname(out), exist_ok=True)
json.dump({"sessions": sessions}, open(out, "w"), indent=1)
print(f"seeded {len(sessions)} sessions")
