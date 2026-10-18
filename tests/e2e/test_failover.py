"""A 402 / exhausted-quota answer switches the turn to the profile's fallback once and goes on; without a fallback it
is an error; a 429 is retried on the same model, after the wait its Retry-After asks for (in seconds or as a date)."""
import email.utils, json, time
from harness import Env, check, done

QUOTA = json.dumps({'error': {'message': 'This API key has reached its spend limit.', 'type': 'key_quota_exceeded',
                              'code': 'key_quota_exceeded'}})
SCRIPT = [{'status': 402, 'body': QUOTA}, {'content': 'answered by the spare'}]

e = Env(SCRIPT, profile='fallback = "spare:spare-model"\n')
r = e.low('--json', 'hello')
check('402 with a fallback: the turn completes', r['result'].get('status') == 'done', r['result'])
models = [q.get('model') for q in r['requests']]
check('the second request goes to the fallback model', models[-1:] == ['spare-model'], models)
check('fallback is not sent as a request parameter', not any('fallback' in json.dumps(q) for q in r['requests']))

e = Env(SCRIPT)
r = e.low('--json', 'hello')
check('402 without a fallback: error, exit 3', r['code'] == 3 and '402' in json.dumps(r['result']), r['result'])

RATE = json.dumps({'error': {'message': 'Rate limit: quota exceeded for requests per minute', 'type': 'rate_limit'}})
e = Env([{'status': 429, 'body': RATE, 'headers': {'Retry-After': '1'}}, {'content': 'after the wait'}],
        profile='fallback = "spare:spare-model"\n')
r = e.low('--json', 'hello')
models = [q.get('model') for q in r['requests']]
check('429 is retried on the same model', r['result'].get('status') == 'done' and models == ['mock-model'] * 2, models)

# An HTTP-date 8 s ahead: the retry waits for it (it used to be read as no Retry-After, and retried after the backoff).
date = email.utils.formatdate(time.time() + 8, usegmt=True)
e = Env([{'status': 429, 'body': RATE, 'headers': {'Retry-After': date}}, {'content': 'after the date'}])
r = e.low('--json', 'hello')
at = [x['at'] for x in e.entries()]
waited = at[1] - at[0] if len(at) == 2 else 0
check('Retry-After as a date is waited for', r['result'].get('status') == 'done' and 6 <= waited < 14, f'{waited:.1f} s')

e = Env([{'content': 'x'}], profile='fallback = "spare-model"\n')
r = e.low('--json', 'hi')
check('fallback without a provider is refused', r['code'] == 3 and 'fallback' in json.dumps(r['result']), r['result'])
done()
