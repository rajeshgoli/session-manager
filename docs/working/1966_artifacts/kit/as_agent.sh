#!/bin/zsh
# #1966: run an sm command as the scratch agent sm-1966-oc (a1966oc0) against the scratch sm.
export SM_API_URL=http://127.0.0.1:18450 CLAUDE_SESSION_MANAGER_ID=a1966oc0 SESSION_MANAGER_ID=a1966oc0
export SM_SESSION_CREDENTIAL=$(python3 -c 'import json;print(json.load(open("/private/tmp/l1966-sm/creds.json"))["a1966oc0"])')
exec sm "$@"
