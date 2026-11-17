#!/usr/bin/env python3
"""Daily audit of what the recall hook injected into Claude Code transcripts."""

import argparse
import json
import random
import re
import subprocess
from datetime import datetime, timedelta, timezone
from pathlib import Path

# ── helpers ───────────────────────────────────────────────────────────────────

# Used only when the record carries no origin.kind. Slash commands and pasted
# text are typed by a person, so they are not here.
_AUTOMATED_PREFIXES = (
    "<task-notification>",
    "[MASTER]",
    "Another Claude session",
    "[SYSTEM",
)
_TOKEN = re.compile(r'[\w\-./:]{3,}')

_JUDGE_PROMPT = """\
You audit a memory system for a coding assistant. Before the assistant answered the user's prompt, the memory system injected the MEMORIES below. Judge each memory against what the assistant actually did in that turn.

Labels:
U = used: the response or tool calls rely on specific information that appears in this memory.
R = relevant: on the same topic or task as the turn and could plausibly help, but the response does not rely on it (or it restates what the turn already shows).
I = irrelevant: a different topic, a stale or generic fact with no bearing on this turn.
M = misleading: the memory contradicts what the turn shows is true now, and the response followed it into an error.

USER PROMPT:
{p}

ASSISTANT RESPONSE (excerpt):
{r}

ASSISTANT TOOL CALLS (excerpt):
{t}

MEMORIES:
{m}

Reply with only a JSON object mapping each memory number to one label, e.g. {{"12":"I","13":"R"}}.
"""


# ── pure functions ────────────────────────────────────────────────────────────

def item_content(item: str) -> str:
    """Extract the content text from a raw memory line.

    Strips the leading ``- `` marker, trailing metadata like `` | When: …``
    and `` [type] (date…)``.
    """
    text = item.strip()
    if text.startswith('- '):
        text = text[2:]
    text = re.sub(r'\s+\[\w+\]\s*\([^)]*\)\s*$', '', text)
    text = re.sub(r' \| .*', '', text)
    return text.strip()


def is_automated(prompt: str) -> bool:
    """Return True when *prompt* starts (after stripping leading whitespace)
    with one of the known machine-message prefixes."""
    stripped = prompt.lstrip()
    return stripped.startswith(_AUTOMATED_PREFIXES)


def _tokens(s: str) -> set[str]:
    return {t.rstrip('.:') for t in _TOKEN.findall(s.lower())} - {''}


def _automated(kind, prompt: str) -> bool:
    """origin.kind decides when present; the prefix rule only fills the gap."""
    if kind == 'human':
        return False
    if kind in ('task-notification', 'peer'):
        return True
    return is_automated(prompt)


def is_known(item: str, prior_text: str) -> bool:
    return _known(item, _tokens(prior_text))


def _known(item: str, prior: set[str]) -> bool:
    """Return True when the item's content was already present in *prior_text*.

    Tokens are compared as whole tokens, not substrings. Tokenisation: ``re.findall(r"[\\w\\-.:/]{3,}", s.lower())``.
    An item must have ≥4 distinct tokens. Anchor tokens — ones with a digit or
    an inner ``_ / . -`` (hashes, paths, identifiers, numbers) — must ALL
    appear, because common words overlap by chance: a commit hash that was
    never mentioned means the fact was not in context. Then ≥70 % of all
    tokens must appear, or ≥90 % when the item has no anchor at all.
    """
    tokens = _tokens(item_content(item))
    if len(tokens) < 4:
        return False
    anchors = [t for t in tokens if re.search(r'\d|\w[_/.\-]\w', t)]
    if any(t not in prior for t in anchors):
        return False
    match = sum(1 for t in tokens if t in prior)
    return match / len(tokens) >= (0.7 if anchors else 0.9)


def _parse_injection_content(raw) -> list[str]:
    """Normalise injection attachment content into a list of lines."""
    if isinstance(raw, list):
        parts = []
        for p in raw:
            if isinstance(p, str):
                p = p.replace('\\n', '\n')
                parts.append(p)
        joined = '\n'.join(parts)
    else:
        joined = raw if isinstance(raw, str) else ''
        joined = joined.replace('\\n', '\n')

    lines = joined.splitlines()
    memories_tag = False
    items: list[str] = []
    for line in lines:
        stripped = line.strip()
        if '<memgarden_memories>' in stripped:
            memories_tag = True
            continue
        if '<memgarden_memories' in stripped and '/' in stripped:
            memories_tag = False
            continue
        if memories_tag and stripped.startswith('- '):
            items.append(stripped)
    return items


def _content(rec: dict):
    msg = rec.get('message')
    return msg.get('content') if isinstance(msg, dict) else None


def _texts(content) -> list[str]:
    """String content, or the str text of every text block in a list."""
    if isinstance(content, str):
        return [content]
    if isinstance(content, list):
        return [b['text'] for b in content
                if isinstance(b, dict) and b.get('type') == 'text' and isinstance(b.get('text'), str)]
    return []


def _origin_kind(origin):
    """origin is a dict on user records but a repr'd dict string on queued_command."""
    if isinstance(origin, dict):
        return origin.get('kind')
    if isinstance(origin, str):
        m = re.search(r"['\"]kind['\"]\s*:\s*['\"]([\w-]+)", origin)
        return m.group(1) if m else None
    return None


def _extract_blocks(record: dict) -> tuple[list[str], list[dict]]:
    """Return (text_blocks, tool_inputs) from an assistant record."""
    blocks = _content(record)
    text_blocks: list[str] = []
    tool_inputs: list[dict] = []
    if isinstance(blocks, list):
        for b in blocks:
            if isinstance(b, dict):
                if b.get('type') == 'text' and isinstance(b.get('text'), str):
                    text_blocks.append(b['text'])
                elif b.get('type') == 'tool_use':
                    tool_inputs.append(b.get('input') or {})
            elif isinstance(b, str):
                text_blocks.append(b)
    elif isinstance(blocks, str):
        text_blocks.append(blocks)
    return text_blocks, tool_inputs


def _prompt_of(rec: dict) -> str | None:
    """The prompt text a user record carries, or None when it is not a prompt.

    Prompts arrive as a plain string, or as a block list with text blocks (an
    image or a queued message). A list of only tool_result blocks is a tool
    reply, and an isMeta record is a prompt only when a peer session sent it.
    """
    if rec.get('type') != 'user':
        return None
    if rec.get('isMeta') is True and _origin_kind(rec.get('origin')) != 'peer':
        return None
    content = _content(rec)
    if isinstance(content, str):
        return content
    texts = _texts(content)
    return '\n'.join(texts) if texts else None


def parse_turns(lines) -> list[dict]:
    """Parse one transcript into turns, ONE TURN PER INJECTION.

    A prompt may trigger several injections (queued messages arrive mid-turn),
    so the unit is the injection attachment, not the prompt. Each turn has:
        ts, prompt, origin – the latest prompt before the injection
        items     – memory lines from that attachment only
        resp      – assistant text after it, until the next injection or prompt
        tools     – tool_use inputs over the same span
        automated – machine-sent prompt (origin.kind or a known prefix)
        prior     – text in context at the injection: everything since the
                    last compaction summary (including it)
    """
    turns: list[dict] = []
    current: dict | None = None
    prior: list[str] = []
    prompt, prompt_ts, prompt_kind = '', '', None

    for line in lines:
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(rec, dict) or rec.get('isSidechain') is True:
            continue

        if rec.get('isCompactSummary') is True:
            prior = _texts(_content(rec))
            current = None
            continue

        attachment = rec.get('attachment')
        text, kind = _prompt_of(rec), _origin_kind(rec.get('origin'))
        if (text is None and isinstance(attachment, dict)
                and attachment.get('type') == 'queued_command'
                and isinstance(attachment.get('prompt'), str)):
            # A message typed or delivered mid-turn: recorded as an attachment,
            # not a user record, and it triggers its own injection.
            text, kind = attachment['prompt'], _origin_kind(attachment.get('origin'))
        if text is not None:
            prompt, prompt_kind = text, kind
            ts = rec.get('timestamp')
            prompt_ts = ts if isinstance(ts, str) else prompt_ts
            prior.append(text)
            current = None
            continue

        if (isinstance(attachment, dict)
                and attachment.get('type') == 'hook_additional_context'
                and '<memgarden_memories>' in str(attachment.get('content', ''))):
            current = {
                'ts': prompt_ts or rec.get('timestamp', ''),
                'prompt': prompt,
                'origin': prompt_kind,
                'items': _parse_injection_content(attachment.get('content', '')),
                'resp': '',
                'tools': [],
                'automated': _automated(prompt_kind, prompt),
                # ponytail: a full copy per injection; ~1.5 GB peak on a month of
                # transcripts. Store a token set instead if that ever matters.
                'prior': '\n'.join(prior),
            }
            turns.append(current)
            continue

        if rec.get('type') == 'assistant':
            text_blocks, tool_inputs = _extract_blocks(rec)
            if current is not None:
                current['resp'] = ' '.join([current['resp']] + text_blocks).strip()
                current['tools'].extend(tool_inputs)
            prior.extend(text_blocks)
            prior.extend(json.dumps(ti, ensure_ascii=False) for ti in tool_inputs)

    return turns


# ── judge ─────────────────────────────────────────────────────────────────────

def _run_judge(turn: dict) -> dict[str, str] | None:
    """Run ``llm ask --hrvl --fast`` with the judge prompt on stdin."""
    p = turn['prompt'][:1500]
    r = ' | '.join(re.split(r'\n+', turn.get('resp', '')))[:2500]
    tools_raw = []
    for ti in turn.get('tools', [])[:6]:
        tools_raw.append(json.dumps(ti, ensure_ascii=False)[:300])
    t = ' ; '.join(tools_raw)[:1200]
    items = turn.get('items', [])
    m = '\n'.join(f'[{i}] {it[:400]}' for i, it in enumerate(items))

    prompt_text = _JUDGE_PROMPT.format(p=p, r=r, t=t, m=m)
    try:
        proc = subprocess.run(
            ['llm', 'ask', '--hrvl', '--fast'],
            input=prompt_text,
            capture_output=True,
            text=True,
            timeout=300,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return None
    if proc.returncode != 0:
        return None
    output = proc.stdout.strip()
    first = output.find('{')
    last = output.rfind('}')
    if first == -1 or last == -1:
        return None
    try:
        return json.loads(output[first:last + 1])
    except json.JSONDecodeError:
        return None


# ── audit ─────────────────────────────────────────────────────────────────────

def audit(turns: list[dict],
          judge=None,
          max_turns: int = 40,
          seed: int = 0) -> dict:
    """Compute audit metrics over *turns*.

    Returns a dict ready to be serialised as a single JSONL line.
    """
    now = datetime.now(timezone.utc)
    result = {
        'turns': 0,
        'items': 0,
        'automated_turns': 0,
        'automated_items': 0,
        'known_items': 0,
        'judged_items': 0,
        'judge_irrelevant': 0,
        'judge_failed_items': 0,
        'judge_labels': {},
        'judged_turns': 0,
        'window_start': now.isoformat(),
        'window_end': now.isoformat(),
        'generated_at': now.isoformat(),
        'version': 1,
    }

    rng = random.Random(seed)
    # Only sample from turns that have items
    turns_with_items = [i for i, t in enumerate(turns) if t.get('items')]
    judge_indices: set[int] = set()
    if judge is not None:
        n = min(max_turns, len(turns_with_items))
        if n > 0:
            judge_indices = set(rng.sample(turns_with_items, n))

    for i, turn in enumerate(turns):
        items = turn.get('items', [])
        if not items:
            continue
        result['turns'] += 1
        result['items'] += len(items)

        prompt = turn.get('prompt', '')
        auto = turn['automated'] if 'automated' in turn else is_automated(prompt)
        if auto:
            result['automated_turns'] += 1
            result['automated_items'] += len(items)

        prior = _tokens(turn.get('prior', ''))
        for it in items:
            if _known(it, prior):
                result['known_items'] += 1

        if i in judge_indices and judge is not None:
            judged_labels = judge(turn)
            if isinstance(judged_labels, dict):
                valid = {str(k) for k in range(len(items))}
                judged_labels = {str(k): str(v).strip().upper()[:1]
                                 for k, v in judged_labels.items() if str(k) in valid}
            if not judged_labels:
                result['judge_failed_items'] += len(items)
            else:
                result['judged_turns'] += 1
                result['judged_items'] += len(judged_labels)
                for lbl in judged_labels.values():
                    if lbl == 'I':
                        result['judge_irrelevant'] += 1
                    result['judge_labels'][lbl] = result['judge_labels'].get(lbl, 0) + 1

    result['judge_calibration'] = {
        'date': '2026-11-18',
        'n': 129,
        'kappa': 0.61,
        'irrelevant_bias_pp': 7,
    }
    return result


# ── CLI ───────────────────────────────────────────────────────────────────────

def main(argv: list[str] | None = None):
    parser = argparse.ArgumentParser(description='Usage audit for MemGarden')
    grp = parser.add_mutually_exclusive_group()
    grp.add_argument('--days', type=int, default=1,
                     help='Window: last N days (default 1)')
    grp.add_argument('--since', type=str, default=None,
                     help='Window: since YYYY-MM-DD')
    parser.add_argument('--max-turns', type=int, default=40)
    parser.add_argument('--seed', type=int, default=None)
    parser.add_argument('--no-judge', action='store_true')
    parser.add_argument('--projects', type=str,
                        default=str(Path.home() / '.claude' / 'projects'))
    parser.add_argument('--out', type=str,
                        default=str(Path.home() / '.local' / 'share' / 'memgarden'
                                    / 'usage-audit.jsonl'))
    args = parser.parse_args(argv)

    now = datetime.now(timezone.utc)
    if args.since:
        window_start = datetime.fromisoformat(args.since)
        if window_start.tzinfo is None:
            window_start = window_start.replace(tzinfo=timezone.utc)
    else:
        window_start = now - timedelta(days=args.days)
    window_end = now
    seed = args.seed if args.seed is not None else int(window_start.date().isoformat().replace('-', ''))

    def in_window(t):
        try:
            ts = datetime.fromisoformat(t['ts'].replace('Z', '+00:00'))
        except (AttributeError, TypeError, ValueError):
            return False
        if ts.tzinfo is None:
            ts = ts.replace(tzinfo=timezone.utc)
        return window_start <= ts <= window_end

    # One parse per file (sessions never share prior text), filtered at once so
    # the prior strings of out-of-window turns are freed file by file.
    turns: list[dict] = []
    for jsonl_file in sorted(Path(args.projects).glob('*/*.jsonl')):
        if jsonl_file.stat().st_mtime < window_start.timestamp():
            continue  # nothing in it can fall inside the window
        with open(jsonl_file, encoding='utf-8', errors='replace') as f:
            turns.extend(t for t in parse_turns(f) if in_window(t))

    judge_fn = None if args.no_judge else _run_judge
    result = audit(turns, judge=judge_fn, max_turns=args.max_turns, seed=seed)
    result['window_start'] = window_start.isoformat()
    result['window_end'] = window_end.isoformat()
    result['generated_at'] = now.isoformat()
    result['version'] = 1

    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    with open(out_path, 'a', encoding='utf-8') as f:
        f.write(json.dumps(result, ensure_ascii=False) + '\n')
    print(json.dumps(result, ensure_ascii=False))


if __name__ == '__main__':
    main()
