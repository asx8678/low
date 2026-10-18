"""No check configured: the verifying run's `assert check()` passes, and finish() ends the turn in that run (verdict
NO CHECK, exit 0). Without edits, finish() is still refused. With a check configured, finish() needs check() to pass
in that run."""
from harness import Env, check, done

FILES = {'calc.py': 'def add(a, b):\n    return a - b\n'}
FINAL = ("accept('add adds')\nedit('calc.py', 'return a - b', 'return a + b')\nimport calc\nassert calc.add(2, 3) == 5\n"
         "assert check()\nfinish('add now adds; verified add(2, 3) == 5')")
e = Env([{'code': FINAL}, {'content': 'the reply low should not need'}], files=FILES)
r = e.low('--json', 'fix add')
check('no check: finish() taken in the verifying run',
      len(r['requests']) == 1 and r['result'].get('answer') == 'add now adds; verified add(2, 3) == 5', r['result'])
check('no check: verdict NO CHECK, exit 0', r['result'].get('verdict') == 'NO CHECK' and r['code'] == 0, r['result'])

e = Env([{'code': "print(open('calc.py').read())\nfinish('it subtracts')"}, {'content': 'It subtracts: calc.py:2.'}],
        files=FILES)
r = e.low('--json', 'what does add do?')
check('no check, no edits: finish() refused', len(r['requests']) == 2 and any('finish() ignored' in t for t in e.tools()),
      e.tools())

e = Env([{'code': FINAL.replace('assert check()\n', '')}, {'content': 'fixed'}], files=FILES,
        low_toml='check = "python3 -c pass"\n')
r = e.low('--json', 'fix add')
check('check configured, not run: finish() refused',
      len(r['requests']) == 2 and any('check() did not pass' in t for t in e.tools()), e.tools())
e = Env([{'code': FINAL}, {'content': 'fixed'}], files=FILES, low_toml='check = "python3 -c pass"\n')
r = e.low('--json', 'fix add')
check('check configured and passed: finish() taken', len(r['requests']) == 1 and r['result'].get('verdict') == 'VERIFIED',
      r['result'])
done()
