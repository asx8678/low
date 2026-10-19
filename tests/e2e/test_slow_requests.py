"""MAX_REQUESTS counts toward the budget note as the tokens do: the model is told after the 150th of 200 requests, and
the 201st asks for the handoff. Slow: 200 runs take about 20 s."""
from harness import Env, check, done, user_messages

HANDOFF = 'HANDOFF: printed 0-199; next: print 200'

e = Env([{'code': f'print({i})', 'prompt': 10, 'cached': 0} for i in range(200)] + [{'content': HANDOFF}])
r = e.low('--json', 'count')
reqs = r['requests']
note = [i for i, q in enumerate(reqs) if any('three quarters of its budget' in m for m in user_messages(q)[-1:])]
check('201 requests: 200 runs and the handoff', len(reqs) == 201, len(reqs))
check('the note follows the 150th', note[:1] == [150] and 'about 50 more requests' in user_messages(reqs[150])[-1],
      note[:3])
check('then the handoff', r['code'] == 5 and r['result'].get('answer') == HANDOFF
      and '200 model requests' in str(r['result'].get('limit')), r['result'])
done()
