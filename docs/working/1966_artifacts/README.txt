Evidence for #1966; read appendix P of ../1966_opencode_local_agents.html.

kit/      The executed scripts. Run output goes to $L1966_RUNS (default ~/.local/share/local-agents-proto/runs-1966).
          judge_service.py, judge_policy.md and proto_proxy.py are #1954's, used unchanged from ../1954_artifacts/kit.
          wall_profile.sh is #1954's with one change: a fifth argument for the agent's own service ports.
          sm_judge.js is the fixed version (absolute paths); runs c and d loaded the earlier one (N.5, denial d-36c4).
          x1.sh is the first opencode experiment (plugin loading, event types, reposted message ids: N.4 finding 1).
briefs/   probe.txt (run a, stand-in model), 1855.md (run c), 1913.md (run d).
evidence/ p1_*            check 1: first submission and the resubmission.
          p2_sm_view      check 2: the scratch sm's view of the agent during the probe brief.
          p3-i1-*, p3-b2-*  check 3: idle and busy deliveries; messages as opencode stored them, sm's view.
          p4-r1..r3, m1   check 4: process restart (r1 lost a message: the pane before its outbox; r2 after),
                          full restart (r3), stand-in model restart (m1); p4-long-status: 12-minute outage.
          p5_*            check 5: owner allow-once record; the plugin path fix.
          local_judge.jsonl, local_judge_allow.jsonl  every judge-service decision in every run
                          (rows with claude_session "ses_check" are the path-fix check, not an agent).
          parent_inbox.jsonl  messages the agents sent their parent through the scratch sm.
          usage_ledger_rows.csv  the scratch sm's message_ledger after all runs (check 6).
          run-a, run-c, run-d  per run: opencode config, bridge config, outbox ledger, pane log, plugin log,
                          usage lines; c and d also metrics, the agent's commit as a patch, and validation logs.
                          run-c/sm_view.jsonl kept sampling into run d; use it only up to 23:38:41Z.
Raw, outside the repo (~/.local/share/local-agents-proto/runs-1966/): opencode databases (<run>/xdg/data),
model request log (model_requests.jsonl), stand-in model log, bridge event logs, and the scratch sm's
config, state and logs (scratch-sm/). Run b (excluded, N.5) is in the raw folder only.
