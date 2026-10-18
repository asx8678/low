"""A long --resume session with a small compact_at: compaction waits until the context is twice what the last one
left (the old rule compacted on nearly every request once the kept part neared compact_at), and every request keeps
the system prompt, the tools, paired tool calls and the task it serves."""
from harness import Env, check, done

FILLER = 'x' * 1500
tasks = [(f'task{k:02} step', f'print("{k}")\nprint("{FILLER}")') for k in range(1, 11)]
script = {'rules': [{'match': t, 'reply': {'reasoning': 'Working on ' + t, 'code': c}} for t, c in tasks],
          'after_tool': {'content': 'Done with that step.'}, 'default': {'content': 'Done.'}}
e = Env(script, profile='compact_at = 3000\n')
codes = [e.low('--json', *(['--resume'] if k else []), t)['code'] for k, (t, _) in enumerate(tasks)]
check('every task completes', codes == [0] * len(tasks), codes)
bodies, log = [x['body'] for x in e.entries()], e.log()
compactions = sum('compact' in x for x in log)
check('compaction does not thrash', 1 <= compactions <= 3, f'{compactions} compactions in {len(bodies)} requests')
broken = []
for k, b in enumerate(bodies):
    m = b['messages']
    if m[0] != bodies[0]['messages'][0] or b['tools'] != bodies[0]['tools']:
        broken.append((k, 'system prompt or tools changed'))
    pending = []
    for msg in m:
        if msg['role'] == 'tool':
            if msg['tool_call_id'] not in pending:
                broken.append((k, 'orphan tool result'))
            else:
                pending.remove(msg['tool_call_id'])
        elif pending:
            broken.append((k, 'a message before the tool results'))
        if msg['role'] == 'assistant':
            pending = [c['id'] for c in msg.get('tool_calls') or []]
    if not any(t in str(u.get('content')) for t, _ in tasks for u in m if u['role'] == 'user'):
        broken.append((k, 'no task in the request'))
check('every request is well formed', not broken, broken)
done()
