"""With no check in low.toml, a new session works one out from the repository's files and adopts it only if it passes
on the tree as found: the system prompt says so, a check() before any change reuses that pass, and a turn's verdict
comes from it. A second session on the same tree reuses the first one's baseline, and a resumed session keeps the
check its start detected without running a baseline. One that fails on the tree as found leaves the session with no
check, and check = "" in low.toml turns detection off."""
from harness import Env, check, done

PASSES = {'README.md': 'hello\n', 'Makefile': 'check:\n\t@grep -q hello README.md\n'}
FAILS = {'README.md': 'hello\n', 'Makefile': 'check:\n\t@echo the tests already fail; exit 1\n'}


def system(r):
    return r['requests'][0]['messages'][0]['content'] if r['requests'] else ''


def baselines(e):
    return [x['check'] for x in e.log() if isinstance(x.get('check'), dict) and x['check'].get('baseline')]


e = Env([{'content': 'Nothing to change.'},
         {'code': 'print(check())'}, {'code': "write('a.txt', 'hello\\n')"}, {'content': 'wrote a.txt'},
         {'code': "write('b.txt', 'hello\\n')"}, {'content': 'wrote b.txt'}], files=PASSES)
r = e.low('--json', 'is there anything to change?')
check('a passing Makefile target is adopted',
      'check() runs `make check`, which low detected from Makefile; it passed on the tree as found.' in system(r),
      system(r)[-300:])
ran = baselines(e)
check('its baseline ran once, in this low', [(b['check'], b['ok'], b['reused']) for b in ran] == [('make check', True, False)],
      ran)
check('the header says where it came from', e.log()[0].get('check') == 'make check' and e.log()[0].get('check_from') == 'Makefile',
      {k: e.log()[0].get(k) for k in ('check', 'check_from', 'check_failed')})

r = e.low('--json', 'write a.txt')
ran = baselines(e)
check('a second session on the same tree reuses that baseline', [(b['ok'], b['reused']) for b in ran] == [(True, True)], ran)
first = e.tools()[0] if e.tools() else ''
check('a check() before any change reuses the pass', 'reused: the tree is unchanged' in first, first)
check('the turn is VERIFIED by the detected check', r['result'].get('verdict') == 'VERIFIED' and r['code'] == 0, r['result'])
check('the check row says it was detected', r['result'].get('check') == 'passed on the final tree (detected from Makefile)',
      r['result'].get('check'))

r = e.low('--json', '--resume', 'write b.txt')
resumed = [x['resume'] for x in e.log() if 'resume' in x]
check('a resumed session runs no baseline', len(baselines(e)) == 1, baselines(e))
check('and keeps the detected check', resumed and resumed[-1].get('check') == 'make check', resumed)
check('which still decides the verdict', r['result'].get('verdict') == 'VERIFIED'
      and r['result'].get('check') == 'passed on the final tree (detected from Makefile)', r['result'])

e = Env([{'code': "write('a.txt', 'hello\\n')"}, {'content': 'wrote a.txt'}], files=FAILS)
r = e.low('--json', 'write a.txt')
check('one that fails on the tree as found is not adopted',
      'No check command is configured' in system(r) and 'make check' not in system(r), system(r)[-300:])
check('the verdict is NO CHECK, naming what failed', r['result'].get('verdict') == 'NO CHECK' and r['code'] == 0
      and 'make check' in r['result'].get('check', '') and 'failed on the tree as found' in r['result'].get('check', ''),
      r['result'])
check('the failure is noted', 'the tests already fail' in [b['out'] for b in baselines(e)][0], baselines(e))

e = Env([{'code': "write('a.txt', 'hello\\n')"}, {'content': 'wrote a.txt'}], files=PASSES, low_toml='check = ""\n')
r = e.low('--json', 'write a.txt')
check('check = "" turns detection off', not baselines(e) and 'No check command is configured' in system(r)
      and 'check_from' not in e.log()[0], baselines(e))
check('and the turn has no check', r['result'].get('verdict') == 'NO CHECK'
      and r['result'].get('check') == 'none configured: set check in low.toml', r['result'])
done()
