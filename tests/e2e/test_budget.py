"""The turn budget weights cached input at a quarter and turn_budget sets its multiplier; session_budget stops a
session's turns, a --tasks run included, without an extra turn; both are validated and never sent to the provider."""
import json, os
from harness import Env, check, done

PROFILE = 'compact_at = 1024\n'  # default ceiling: 60 x 1024 = 61,440
runs = lambda n, **usage: [{'code': f'print({i})', **usage} for i in range(n)] + [{'content': 'done'}]

# 6 requests of 30k prompt, 29k cached: raw 180k would stop the turn after 2; weighted ~8.3k each finishes.
e = Env(runs(6, prompt=30000, cached=29000), profile=PROFILE)
r = e.low('--json', 'count')
check('well-cached turn completes', r['result'].get('status') == 'done', r['result'])
check('all six runs happened', len(r['requests']) == 7, len(r['requests']))

# Uncached: 30k fresh each stops after 3 requests (90k > 61k).
e = Env(runs(6, prompt=30000, cached=0), profile=PROFILE)
r = e.low('--json', 'count')
check('uncached turn stops at the budget', r['result'].get('status') == 'stopped' and r['code'] == 5, r['result'])
check('limit text explains the rule', 'cached input counts a quarter' in str(r['result'].get('limit')), r['result'])

# turn_budget = 200 lets the same uncached turn finish (200 x 1024 = 204,800 > 6 x 30k).
e = Env(runs(6, prompt=30000, cached=0), profile=PROFILE + 'turn_budget = 200\n')
r = e.low('--json', 'count')
check('turn_budget raises the ceiling', r['result'].get('status') == 'done', r['result'])
check('turn_budget is not sent to the provider', not any('turn_budget' in json.dumps(q) for q in r['requests']))

for key in ('turn_budget', 'session_budget'):
    e = Env([{'content': 'x'}], profile=PROFILE + f'{key} = 0\n')
    r = e.low('--json', 'hi')
    check(f'{key} = 0 is refused', r['code'] == 3 and key in json.dumps(r['result']), r['result'])

# session_budget = 1 (1024 tokens): the first task's turn spends past it while it is still writing files, so the run
# stops there with exit 5 and the checklist says why. It gets no second turn (one that stopped at once, with no
# request, and hid the first one's work).
step = lambda n: {'code': f'write("t{n}.txt", "{n}\\n")'}
e = Env([step(1), step(2), step(3)], profile=PROFILE + 'session_budget = 1\n',
        files={'TODO.md': '- [ ] one\n- [ ] two\n'})
r = e.low('--json', '--tasks', 'TODO.md')
tasks = [(t.get('task'), t['result'].get('status')) for t in r['result'].get('tasks', [])]
check('session_budget: --tasks stops with exit 5', r['code'] == 5, r['result'])
check('session_budget: one turn ran, no continuation', tasks == [('one', 'stopped')] and len(r['requests']) == 1,
      f'{tasks}, {len(r["requests"])} requests')
check("session_budget: the turn's work is kept", e.read('t1.txt') == '1\n')
body = e.read('TODO.md')
check('session_budget: the checklist line says why it stopped', 'session_budget' in body, body)
check('session_budget is not sent to the provider', not any('session_budget' in json.dumps(q) for q in r['requests']))
done()
