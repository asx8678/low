"""The turn budget weights cached input at a quarter. The budget keys low no longer reads are refused, never sent to
the provider."""
import json
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

for key in ('turn_budget', 'session_budget', 'keep_warm', 'fallback'):
    e = Env([{'content': 'x'}], profile=PROFILE + f'{key} = 1\n')
    r = e.low('--json', 'hi')
    check(f'{key} is refused', r['code'] == 3 and key in json.dumps(r['result']) and not r['requests'], r['result'])
done()
