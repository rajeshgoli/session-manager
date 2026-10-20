"""Aggregate all frames (without truncation) across sample captures."""

import collections
import sys
from prof import profile


def aggregate(paths):
    busy = 0
    groups = {name: collections.Counter() for name in ('top', 'deepest', 'mutex')}
    for path in paths:
        result = profile(path)
        busy += result['busy']
        for name, counts in groups.items():
            counts.update(result[name])
    print(f'{len(paths)} profiles; {busy} busy samples')
    print('Busy cores are not inferred: sample intervals stretch under load.')
    for name, counts in groups.items():
        print(f'-- {name}')
        for frame, count in counts.most_common():
            print(f'{100 * count / busy if busy else 0:6.2f}% {count:9d} {frame}')


if __name__ == '__main__':
    if len(sys.argv) < 2:
        sys.exit('Usage: agg.py SAMPLE.txt ...')
    aggregate(sys.argv[1:])
