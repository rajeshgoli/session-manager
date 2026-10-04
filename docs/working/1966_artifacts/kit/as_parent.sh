#!/bin/zsh
# #1966: run an sm command as the scratch parent sm-1966 (p1966000) against the scratch sm.
export SM_API_URL=http://127.0.0.1:18450 CLAUDE_SESSION_MANAGER_ID=p1966000 SESSION_MANAGER_ID=p1966000
export SM_SESSION_CREDENTIAL=$(python3 -c 'import json;print(json.load(open("/private/tmp/l1966-sm/creds.json"))["p1966000"])')
exec sm "$@"
