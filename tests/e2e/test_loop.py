"""Reasoning that goes round in a loop is cut off as it streams, long before the output limit: the reply is dropped, the
model is told why and asked to act, the next request starts with the looping one's messages, and the estimate counts
in the turn's tokens. A third unusable reply in a row stops the turn (exit 5)."""
import json
from harness import Env, check, done, user_messages

VARIED = ''.join(f'Step {i}: {i * 7919 % 10007} is checked against {i * 104729 % 1009}. ' for i in range(300))
UNIT = 'Need maybe check `cast` in the changeset again: raw string -> invalid. Good.\n'
LOOP = VARIED + UNIT * 2500  # 200 KB, about what DeepSeek sends before its 64k-token limit
LOOPING = {'reasoning': LOOP, 'chunk': 4096}

e = Env([LOOPING, {'code': "print('next step')"}, {'content': 'done'}])
r = e.low('--json', 'check the changesets')
reqs = r['requests']
loops = [x for x in e.log() if x.get('event') == 'loop']
check('the turn goes on to its answer', r['code'] == 0 and r['result'].get('answer') == 'done', r['result'])
check('three requests: the loop, the next step, the answer', len(reqs) == 3, len(reqs))
check('the loop is cut off within a window of it', len(loops) == 1 and len(VARIED) < loops[0]['chars'] < len(VARIED) + 20000,
      [(x['text'], x['chars']) for x in loops])
nudge = user_messages(reqs[1])[-1] if len(reqs) > 1 else ''
check('the model is told why', 'kept repeating itself' in nudge, nudge)
if len(reqs) > 1:
    # The nudge joins the task it follows, as one user message.
    a, b = reqs[0]['messages'], reqs[1]['messages']
    check('the next request starts with the looping one',
          b[:len(a) - 1] == a[:-1] and b[len(a) - 1]['content'].startswith(a[-1]['content']), b[len(a) - 1:])
    check('the loop is not sent back', UNIT not in json.dumps(reqs[1:]), '')
check('the loop is not kept', not any(UNIT in (x.get('reasoning_content') or '') for x in e.log()), '')
estimate = loops[0]['meta']['usage']['completion_tokens'] if loops else 0
check('its estimate counts in the turn', estimate > 4000 and r['result'].get('tokens', {}).get('out', 0) >= estimate,
      (estimate, r['result'].get('tokens')))

# Mid-turn, after a run's result, as the loops in the logs came: the nudge is a message of its own.
e = Env([{'code': "print('looked')"}, LOOPING, {'content': 'done'}])
r = e.low('--json', 'check the changesets')
reqs = r['requests']
check('mid-turn: the turn goes on', r['code'] == 0 and r['result'].get('answer') == 'done' and len(reqs) == 3, r['result'])
if len(reqs) == 3:
    a, b = reqs[1]['messages'], reqs[2]['messages']
    check('mid-turn: the next request is the looping one and the nudge', b[:len(a)] == a and len(b) == len(a) + 1
          and 'kept repeating itself' in b[-1]['content'], b[len(a):])

e = Env([LOOPING, LOOPING, LOOPING, {'content': 'not reached'}])
r = e.low('--json', 'check the changesets')
check('a third loop in a row stops the turn', r['code'] == 5 and len(r['requests']) == 3
      and 'kept repeating itself' in str(r['result'].get('limit')), (r['code'], len(r['requests']), r['result'].get('limit')))
check('each loop is logged', len([x for x in e.log() if x.get('event') == 'loop']) == 3, '')
done()
