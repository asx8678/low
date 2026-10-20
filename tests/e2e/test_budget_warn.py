"""Once a turn has used three quarters of its budget, the model is told about how many requests it has left, once, and
asked to spend them on the change, not to end the turn; the limit still stops the turn. A turn-scoped limit then gets
one last request for a handoff, with no code run, which becomes the turn's answer: a --tasks task cut off while still
changing files keeps its extra turns, and starts them from that answer."""
import os
from harness import Env, check, done, user_messages

PROFILE = 'compact_at = 1024\n'  # turn ceiling = 60 x 1024 = 61,440 tokens
TEN = {'prompt': 10000, 'cached': 0}  # each request weighs 10,010: 10,000 fresh and 10 out
WARN = 'three quarters of its budget'
ASK = 'reached its limit'
HANDOFF = 'HANDOFF: printed 0-6; left: the rest; next: print 7'


def says(body, text):
    return sum(text in m for m in user_messages(body))


# Requests 1-4 bring the turn to 40k (65%), the 5th to 50k (81%): the note follows it, with about 2 left
# ((61,440 - 50,050) / 10,010). The 7th passes the ceiling, and the 8th asks for the handoff.
e = Env([{'code': f'print({i})', **TEN} for i in range(7)] + [{'content': HANDOFF}], profile=PROFILE)
r = e.low('--json', 'count')
reqs = r['requests']
check('eight requests: seven runs and the handoff', len(reqs) == 8, len(reqs))
check('no note before three quarters', all(says(q, WARN) == 0 for q in reqs[:5]), [says(q, WARN) for q in reqs])
note = next((m for m in user_messages(reqs[5]) if WARN in m), '') if len(reqs) > 5 else ''
check('the note follows the request that crossed it', bool(note), user_messages(reqs[5])[-1:] if len(reqs) > 5 else '')
check('it gives the requests left', 'about 2 more requests' in note, note)
check('it asks for focus, not an end', 'Spend them on the change' in note and 'finish' not in note, note)
check('it is sent once', says(reqs[-1], WARN) == 1, says(reqs[-1], WARN))
check('the limit still stops the turn', r['code'] == 5 and r['result'].get('status') == 'stopped'
      and 'stopped after' in str(r['result'].get('limit')), r['result'])
check('the handoff is asked for last', len(reqs) == 8 and ASK in user_messages(reqs[7])[-1],
      user_messages(reqs[-1])[-1:])
check("the handoff is the turn's answer", r['result'].get('answer') == HANDOFF, r['result'].get('answer'))
events = [x.get('event') for x in e.log() if x.get('event')]
check('the note is logged as a budget event', events.count('budget') == 1, events)
end = next((x['end'] for x in e.log() if 'end' in x), {})
check('the end tallies it', end.get('events', {}).get('budget') == 1, end.get('events'))

# A turn that ends under three quarters sends nothing new.
e = Env([{'code': 'print(1)', **TEN}, {'code': 'print(2)', **TEN}, {'content': 'done'}], profile=PROFILE)
r = e.low('--json', 'count')
check('under three quarters: done, three requests', r['code'] == 0 and len(r['requests']) == 3, r['result'])
check('under three quarters: no note, no handoff',
      not any(says(q, WARN) or says(q, ASK) for q in r['requests']), [user_messages(q)[-1] for q in r['requests']])

# A handoff reply that calls run() runs nothing, and its call gets a result, so the next request is well formed.
e = Env([{'code': f'print({i})', **TEN} for i in range(7)]
        + [{'content': 'one last write', 'code': "write('late.txt', 'x\\n')"}, {'content': 'resumed'}], profile=PROFILE)
r = e.low('--json', 'count')
check('a run() in the handoff is not run', not os.path.exists(os.path.join(e.repo, 'late.txt')))
check('it gets a not-run result', e.tools()[-1:] == ['not run: this turn had reached its limit'], e.tools()[-1:])
check('the turn is still stopped, by the budget', r['code'] == 5 and 'stopped after' in str(r['result'].get('limit')),
      r['result'])
r = e.low('--json', '--resume', 'go on')
last = r['requests'][0]['messages'] if r['requests'] else []
calls = [c['id'] for m in last if m['role'] == 'assistant' for c in m.get('tool_calls') or []]
answered = {m['tool_call_id'] for m in last if m['role'] == 'tool'}
check('the next turn answers every call', r['code'] == 0 and set(calls) <= answered, (r['code'], calls, answered))

# MAX_REQUESTS's note and handoff are in test_slow_requests.py.

# --tasks: a task still writing files when the ceiling stops it gets its handoff, then its extra turn as before, and
# the extra turn has the handoff in its context.
w = lambda n, **u: {'code': f'write("t{n}.txt", "{n}\\n")', **u}
e = Env([w(i, **TEN) for i in range(7)] + [{'content': HANDOFF}, w(7), {'content': 'built'}, {'content': 'second'}],
        profile=PROFILE, files={'TASKS.md': '- [ ] build it\n- [ ] then this\n'})
r = e.low('--json', '--tasks', 'TASKS.md')
tasks = [(t.get('task'), t['result'].get('status')) for t in r['result'].get('tasks', [])]
check('--tasks: the cut task runs again', tasks[:2] == [('build it', 'stopped'), ('build it', 'done')], tasks)
check('--tasks: every task is done', r['result'].get('left') == 0 and r['code'] == 0, r['result'])
again = r['requests'][8]['messages'] if len(r['requests']) > 8 else []
check('--tasks: the extra turn sees the handoff', any(m['role'] == 'assistant' and m.get('content') == HANDOFF
                                                      for m in again), [m.get('content') for m in again][-3:])
done()
