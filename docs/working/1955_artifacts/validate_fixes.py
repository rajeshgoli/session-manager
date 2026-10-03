"""Validate local and merged regression tests, including failures without fixes."""
import json
import os
from pathlib import Path
import subprocess

ROOT = Path('/private/tmp/sm-1955')
REPO = '/Users/rajesh/projects/session-manager'
env = {**os.environ, 'CARGO_NET_OFFLINE': 'true'}
results = []


def command(work, label, args, expect_failure=False):
    target = ROOT / ('1913' if work.name.endswith('1913') else '1855') / 'validation'
    target.mkdir(exist_ok=True)
    with (target / (label + '.log')).open('w') as log:
        process = subprocess.run(args, cwd=work, env=env, stdout=log, stderr=subprocess.STDOUT)
    content = (target / (label + '.log')).read_text()
    summaries = [line for line in content.splitlines() if 'test result:' in line or line.startswith('error:')]
    valid = process.returncode != 0 if expect_failure else process.returncode == 0
    if expect_failure:
        valid = valid and 'FAILED' in content and '0 passed; 0 failed' not in content
    record = dict(ticket=int(work.name.rsplit('-', 1)[1]), label=label, exit=process.returncode,
                  expect_failure=expect_failure, valid=valid, summaries=summaries)
    results.append(record)
    print(json.dumps(record), flush=True)
    (ROOT / 'validation-results.json').write_text(json.dumps(results, indent=2) + '\n')
    if not valid:
        raise RuntimeError(f'{label}: unexpected test result; see {target}')


for ticket in [1913, 1855]:
    work = Path(f'/Users/rajesh/worktrees/sm-1955-opencode-{ticket}')
    if subprocess.check_output(['git', '-C', str(work), 'status', '--porcelain']):
        raise RuntimeError(f'refusing to overwrite dirty benchmark clone {work}')
    head = subprocess.check_output(['git', '-C', str(work), 'rev-parse', 'HEAD'], text=True).strip()
    runner = ['scripts/test-rust-isolated.sh', '-p', 'sm-server']
    try:
        if ticket == 1913:
            own = 'queue_job_child_inherits_nothing_above_standard_descriptors'
            merged = 'queue_job_does_not_inherit_handed_over_listeners'
            command(work, 'A-own-test', runner + ['--lib', own])
            command(work, 'B-queue-module', runner + ['--lib', 'queue::tests'])
            patch = subprocess.check_output(['git', '-C', REPO, 'show', '--format=', '14ea14c936', '--', 'crates/sm-server/src/queue.rs'])
            subprocess.run(['git', '-C', str(work), 'apply', '--whitespace=nowarn'], input=patch, check=True)
            command(work, 'C-merged-test', runner + ['--lib', merged])
            source = work / 'crates/sm-server/src/queue.rs'
            before = source.read_text()
            fix = '        crate::runtime::close_inherited_descriptors_before_exec(&mut command);\n'
            assert before.count(fix) == 1
            source.write_text(before.replace(fix, ''))
            command(work, 'D-own-test-without-fix', runner + ['--lib', own], True)
            command(work, 'E-merged-test-without-fix', runner + ['--lib', merged], True)
        else:
            own = 'request_codex_review_is_a_hidden_alias_of_request_review'
            command(work, 'A-own-test', runner + ['--bin', 'sm', own])
            merged_file = work / 'crates/sm-server/tests/retired_review_name.rs'
            assert not merged_file.exists()
            merged_file.write_bytes(subprocess.check_output(['git', '-C', REPO, 'show', '2100e916:crates/sm-server/tests/retired_review_name.rs']))
            command(work, 'B-merged-test', runner + ['--test', 'retired_review_name'])
            source = work / 'crates/sm-server/src/bin/sm.rs'
            original = source.read_text()
            alias = '#[command(name = "request-review", alias = "request-codex-review")]'
            assert original.count(alias) == 1
            baseline = subprocess.check_output(['git', '-C', str(work), 'show', '4a3a060d6c37:crates/sm-server/src/bin/sm.rs'], text=True)
            block = baseline.split('fn run() -> Result<()> {\n', 1)[1].split('    let cli = Cli::parse();', 1)[0]
            unfixed = original.replace(alias, '#[command(name = "request-review")]')
            source.write_text(unfixed.replace('fn run() -> Result<()> {\n', 'fn run() -> Result<()> {\n' + block))
            command(work, 'C-own-test-without-fix', runner + ['--bin', 'sm', own], True)
            command(work, 'D-merged-test-without-fix', runner + ['--test', 'retired_review_name'], True)
    finally:
        # This clone was verified clean and only this script edits it during validation.
        subprocess.run(['git', '-C', str(work), 'reset', '--hard', head], check=True, stdout=subprocess.DEVNULL)
        if ticket == 1855:
            (work / 'crates/sm-server/tests/retired_review_name.rs').unlink(missing_ok=True)
    command(work, 'F-fmt', ['cargo', 'fmt', '-p', 'sm-server', '--check'])
    command(work, 'G-clippy', ['cargo', 'clippy', '-p', 'sm-server', '--all-targets', '--', '-D', 'warnings'])
