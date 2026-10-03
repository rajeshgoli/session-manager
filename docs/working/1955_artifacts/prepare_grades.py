"""Prepare independent grading packets required by appendix H.5."""
import json
from pathlib import Path
import subprocess

root = Path('/private/tmp/sm-1955')
repo = '/Users/rajesh/projects/session-manager'
for ticket, base, merged, minutes in [(1913, '0d3211e3fdb1', '14ea14c936', 14), (1855, '4a3a060d6c37', '2100e916', 7)]:
    work = Path(f'/Users/rajesh/worktrees/sm-1955-opencode-{ticket}')
    grade = root / str(ticket) / 'grade'
    grade.mkdir(exist_ok=True)
    (grade / 'issue-and-brief.txt').write_bytes((root / str(ticket) / 'baseline-brief.md').read_bytes())
    (grade / 'local.diff').write_bytes(subprocess.check_output(['git', '-C', str(work), 'diff', base, 'HEAD']))
    (grade / 'merged.diff').write_bytes(subprocess.check_output(['git', '-C', repo, 'show', merged]))
    results = [r for r in json.loads((root / 'validation-results.json').read_text()) if r['ticket'] == ticket]
    (grade / 'validation.json').write_text(json.dumps(results, indent=2))
    brief = f'''You independently grade the Qwen3.8-Flash-Next/opencode benchmark for ticket #{ticket}, following appendix H.5 of docs/working/1784_local_agents.html. This is a benchmark grade, not a PR review. Tier: Mid; model Sol; effort medium. Do the grade inline; do not delegate. Read all of this brief.

Evidence packet: {grade}. issue-and-brief.txt is the exact original issue/agent brief. local.diff is the agent's change. merged.diff is the actual merged fix, originally completed in about {minutes} minutes. validation.json records driver checks. Full test logs are in {root / str(ticket) / 'validation'}. Both the agent's own and merged regression tests pass with its fix and fail with the fix removed. Fmt and clippy pass. The agent's checkout is {work}; read surrounding code there as needed. The initial agent ran without live sm or internet, inside the prototype macOS sandbox. It committed its fix and stopped. Driver validation is complete; do not rerun builds.

Do not edit any source, commit, push, claim a ticket, open a PR, or post GitHub comments. Work read-only. Do not read the other grader's packet or any benchmark speed metrics; judge quality independently. Use the issue, local diff, merged diff and test logs rather than assuming the merged implementation is the only correct implementation.

Answer exactly these six points with concrete reasoning:
1. Does the local change fix the reported bug? Yes, partly, or no, and why.
2. Does it add a test that fails before its fix and passes after? Assess the test's real coverage.
3. Does it stay in scope?
4. How does its quality compare with the merged fix? Identify observable differences.
5. Verdict: mergeable as-is, small changes, large changes, or wrong.
6. Rescue effort: estimated fraction (0–100%) of the original agent's work still needed to bring it to the merged fix's standard; explain the work. This number is recorded, not gating.

Send all six answers in one message and then stand by:
sm send sm-1955 <<'GRADE'
<six numbered answers>
GRADE
'''
    (grade / 'brief.txt').write_text(brief)
    print(grade / 'brief.txt')
