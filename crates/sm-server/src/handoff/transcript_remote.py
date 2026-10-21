"""Extract the bounded owner-message suffix before sending it over SSH."""
import bisect
import collections
import json
import math
import os
import re
import sys

# Match Rust str::split_whitespace/trim_start, including its Unicode whitespace.
whitespace = re.compile(r'[\u0009-\u000d\u0020\u0085\u00a0\u1680\u2000-\u200a\u2028\u2029\u202f\u205f\u3000]+')


def normalize(text):
    return ' '.join(part for part in whitespace.split(text) if part)


def field(value, key):
    return value.get(key) if isinstance(value, dict) else None


def invalid_constant(value):
    raise ValueError('invalid JSON number: ' + value)


def finite_number(value):
    number = float(value)
    if not math.isfinite(number):
        invalid_constant(value)
    return number


def validate_strings(value):
    # Python accepts unpaired escaped surrogates; serde_json rejects them even
    # in tool records that do not contribute any owner text.
    if isinstance(value, str):
        value.encode('utf-8')
    elif isinstance(value, dict):
        for key, item in value.items():
            validate_strings(key)
            validate_strings(item)
    elif isinstance(value, list):
        for item in value:
            validate_strings(item)


def extract(request):
    excluded = set(request['excluded'])
    prefixes = ('[Input from: ', '[sm ', *request['harness_tags'])
    # Queued prompts no user record has consumed yet, kept until one decides.
    enqueued = collections.defaultdict(collections.deque)
    last_codex = None
    kept = []
    size = 0
    cutoff = None

    def keep(at, sequence, text):
        nonlocal size, cutoff
        key = (at, sequence)
        if cutoff is not None and key <= cutoff:
            return
        entry = (at, sequence, text, len(text.encode('utf-8')))
        bisect.insort(kept, entry)
        size += entry[3]
        # Stable timestamp order matches the final Rust merge. The cutoff
        # preserves the first non-fitting message as a barrier even when
        # timestamps arrive out of order. Never split the newest message.
        while size > 80_000 and len(kept) > 1:
            removed = kept.pop(0)
            size -= removed[3]
            cutoff = removed[:2]

    try:
        source = open(os.path.expanduser(request['path']), encoding='utf-8')
    except FileNotFoundError:
        return {'messages': [], 'cutoff': None}
    with source:
        for sequence, line in enumerate(source):
            if not normalize(line):
                continue
            event = json.loads(line, parse_constant=invalid_constant,
                               parse_float=finite_number, parse_int=finite_number)
            validate_strings(event)
            kind = field(event, 'type')
            payload = field(event, 'payload')
            queue = kind == 'queue-operation' and field(event, 'operation') == 'enqueue'
            user = (kind == 'user'
                    or (kind == 'response_item' and field(payload, 'type') == 'message'
                        and field(payload, 'role') == 'user')
                    or (kind == 'event_msg' and field(payload, 'type') == 'user_message'))
            if not queue and not user:
                continue
            if queue:
                content = field(event, 'content')
            elif kind == 'user':
                content = field(field(event, 'message'), 'content')
            elif kind == 'event_msg':
                content = field(payload, 'message')
            else:
                content = field(payload, 'content')
            if isinstance(content, str):
                text = content
            elif isinstance(content, list):
                text = '\n'.join(field(p, 'text') for p in content
                                 if field(p, 'type') in ('text', 'input_text')
                                 and isinstance(field(p, 'text'), str))
            else:
                continue
            normalized = normalize(text)
            trimmed = whitespace.sub('', text, count=1) if whitespace.match(text) else text
            if not normalized or trimmed.startswith(prefixes) or normalized in excluded:
                continue
            at = field(event, 'timestamp')
            at = at if isinstance(at, str) else ''
            if queue:
                enqueued[normalized].append((at, sequence, text))
                continue
            if enqueued[normalized]:
                queued = enqueued[normalized].popleft()
                # Claude marks prompts the harness made (a CronCreate fire, a
                # permission grant, an image caption) isMeta: never the owner's.
                if field(event, 'isMeta') is not True:
                    keep(*queued)
                continue
            if field(event, 'isMeta') is True:
                continue
            if kind in ('response_item', 'event_msg'):
                if last_codex is not None and last_codex[0] != kind and last_codex[1] == normalized:
                    last_codex = None
                    continue
                last_codex = (kind, normalized)
            keep(at, sequence, text)
    # A queued prompt Claude absorbed mid-turn has no user record.
    for queued in enqueued.values():
        for entry in queued:
            keep(*entry)
    return {'messages': [[at, text] for at, _, text, _ in kept],
            'cutoff': cutoff[0] if cutoff is not None else None}


# Emit nothing on parse/read failure: the caller drops the whole transcript.
print(json.dumps(extract(json.load(sys.stdin)), ensure_ascii=False))
