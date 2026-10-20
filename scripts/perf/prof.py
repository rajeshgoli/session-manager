"""Attribute macOS sample call graphs using memo #2416 Appendix A."""

import collections
import json
import re
import sys

WAIT = {
    '__psynch_cvwait', '__semwait_signal', 'poll', 'kevent', '__ulock_wait',
    '__ulock_wait2', '__workq_kernreturn', 'mach_msg2_trap', '__select',
}
NODE = re.compile(r'^([ +!:|]*)(\d+) (\S+)')


def clean(name):
    return re.sub(r'::h[0-9a-f]+$', '', name).replace(
        '$u7b$$u7b$closure$u7d$$u7d$', '{closure}')


def profile(path):
    with open(path) as source:
        lines = source.read().splitlines()
    start = lines.index('Call graph:')
    end = next(i for i in range(start + 1, len(lines))
               if lines[i].startswith('Total number in stack'))
    top, deepest, mutex = (collections.Counter() for _ in range(3))
    stack = []

    def finish():
        _, count, name, children = stack.pop()
        own = count - children
        frames = [entry[2] for entry in stack] + [name]
        app = [frame for frame in frames if frame.startswith('sm_server::')
               and frame.split('::')[1] not in {'main', 'run_background_pass'}]
        if own <= 0:
            return
        if name == '__psynch_mutexwait':
            mutex[app[-1] if app else '(non-sm)'] += own
        elif name not in WAIT:
            top[app[0] if app else '(non-sm)'] += own
            deepest[app[-1] if app else '(non-sm)'] += own

    for line in lines[start + 1:end]:
        match = NODE.match(line)
        if not match:
            continue
        depth, count, name = len(match[1]), int(match[2]), clean(match[3])
        while stack and stack[-1][0] >= depth:
            finish()
        if stack:
            stack[-1][3] += count
        stack.append([depth, count, name, 0])
    while stack:
        finish()
    return dict(busy=sum(top.values()), top=dict(top), deepest=dict(deepest),
                mutex=dict(mutex))


if __name__ == '__main__':
    print(json.dumps(profile(sys.argv[1]), indent=2))
