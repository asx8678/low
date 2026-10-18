"""Paths in low.toml's protect (or --protect) can't be changed by the model's edit()/write() or its shell commands;
everything else stays writable; protect in the user config is refused."""
import json, os, subprocess
from harness import Env, LOW, check, done

FILES = {'README.md': 'hi\n', 'shared.txt': 'shared\n', 'locked/a.txt': 'a\n', 'free/b.txt': 'b\n'}
CODE = """
for attempt in (lambda: write('shared.txt', 'changed'), lambda: sh('rm -rf locked'), lambda: write('mine.txt', 'mine')):
    try:
        print(attempt())
    except Exception as e:
        print('ERR', e)
"""
e = Env([{'code': CODE}, {'content': 'tried'}], files=FILES, low_toml='protect = ["shared.txt", "locked"]\n')
e.low('--json', 'try')
out = (e.tools() or [''])[-1]
check('write() to a protected file is refused', e.read('shared.txt') == 'shared\n' and 'protected' in out, out)
check('rm of a protected folder is blocked', os.path.exists(os.path.join(e.repo, 'locked/a.txt')), out)
check('other paths stay writable', e.read('mine.txt') == 'mine', out)

e = Env([{'code': "write('free/b.txt', 'x')"}, {'content': 'tried'}], files=FILES)
e.low('--json', '--protect', 'free', 'try')
check('--protect protects for this run', e.read('free/b.txt') == 'b\n')

e = Env([{'content': 'x'}], config='protect = ["x"]')
r = e.low('--json', 'hi')
check('protect in the user config is refused', r['code'] == 3 and 'low.toml' in json.dumps(r['result']), r['result'])

bad = subprocess.run([LOW, '--send', '--protect', 'x', 'task'], capture_output=True, text=True, env=e.env())
check('--send with --protect exits 2', bad.returncode == 2, bad.stderr)
done()
