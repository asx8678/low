"""search() and fetch() take a list too: low makes the requests at once, 10 at a time, and gives the answers back in
order, a refused or failed one as its RuntimeError in its place. A list holds up to 50; an empty one asks nothing."""
from harness import Env, check, done

SLOW = 'https://registry.npmjs.org/slow/{}'
CODE = r'''
import time
t = time.time()
five = fetch([SLOW.format(n) for n in range(5)])
print("five", round(time.time() - t, 1), five)
t = time.time()
twelve = fetch([SLOW.format(n) for n in range(5, 17)])
print("twelve", round(time.time() - t, 1), twelve == [f"slow page {n}" for n in range(5, 17)])
mixed = fetch(["https://registry.npmjs.org/three/latest", "https://example.org/made-up",
               "https://registry.npmjs.org/missing"])
print("mixed", [type(m).__name__ for m in mixed])
print("refused", mixed[1])
print("missing", mixed[2])
print("search", [[r["title"] for r in found] for found in search(["alpha", "beta"], count=1)])
try:
    fetch([SLOW.format(n) for n in range(51)])
except ValueError as e:
    print("too many", e)
print("empty", fetch([]), search(()))
print("past 50", search.__globals__["_web"]([{"fetch": "https://registry.npmjs.org/three/latest"}] * 51)[49:])
'''.replace('SLOW', repr(SLOW))
e = Env([{'code': CODE}, {'content': 'done'}], search=True)
r = e.low('--json', 'read the release pages')
out = (e.tools() or [''])[-1]
line = lambda name: next((l for l in out.splitlines() if l.startswith(name + ' ')), '')
asked = [x for x in e.entries() if 'tool' in x]
# What left low: the tools API's fetches, and the open hosts' URLs that low asks itself.
fetched = [a for a in asked if a['tool'] in ('fetch', 'open')]
system = next((b['messages'][0]['content'] for b in r['requests'] if 'messages' in b), '')

check('the turn completes', r['code'] == 0 and r['result'].get('answer') == 'done', r['result'])
check('the prompt shows the list form', 'search([query, ...]), fetch([url, ...]) -> list' in system, system[-1500:])
check('five slow pages come back in order, in about the time of one',
      line('five').endswith(str([f'slow page {n}' for n in range(5)])) and float(line('five').split()[1]) < 3, out)
at = sorted(a['at'] for a in fetched[5:17])
check('twelve go out 10 at a time',
      line('twelve').endswith('True') and at[9] - at[0] < 0.5 and at[10] - at[0] > 0.8, (line('twelve'), at))
check('a refused URL and a failed one are RuntimeErrors in their places, between pages that came back',
      line('mixed') == "mixed ['str', 'RuntimeError', 'RuntimeError']" and 'registry.npmjs.org' in line('refused')
      and '404' in line('missing'), out)
check('the refused URL never leaves the machine', not any('example.org' in a['body'].get('url', '') for a in fetched),
      fetched)
check('a list of searches answers each query in its place',
      line('search') == "search [['alpha: https://example.org/three/releases'], "
                        "['beta: https://example.org/three/releases']]", out)
check('51 at once is refused before anything is asked',
      'up to 50 go at once' in line('too many') and sum('/slow/' in a['body']['url'] for a in fetched) == 5 + 12,
      (line('too many'), len(fetched)))
check('an empty list asks nothing', line('empty') == 'empty [] []', out)
check("low refuses past 50 itself, for code that goes around the helpers",
      line('past 50').startswith('past 50 [[True, ') and line('past 50').endswith("[False, 'only 50 requests go in one list']]")
      and len(fetched) == 5 + 12 + 2 + 50, (line('past 50')[-200:], len(fetched)))
done()
