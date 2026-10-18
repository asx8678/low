"""(slow: 5+ minutes) Keep-alive comments count as the provider being alive: a reply that comes after 310 s of them
is taken, where the old build gave up at 300 s without a data event."""
from harness import Env, check, done

e = Env([{'keepalive': [10, 310], 'content': 'hi there'}])
r = e.low('--json', 'say hi', timeout=500)
check('a reply after 310 s of keep-alives is taken', r['code'] == 0 and r['result'].get('answer') == 'hi there', r['result'])
done()
