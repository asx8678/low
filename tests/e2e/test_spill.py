"""A run whose output is cut keeps its whole output in a file outside the tree, inside the sandbox: the cut names it,
the next run reads the middle with show(), and the repo stays as it was, so no stray-file question follows. Each
output has a name no other worker's takes, and a worker deletes only its own older ones and others' a day old."""
from harness import Env, check, done

LONG = "print('\\n'.join(f'line {i} ' + 'x' * 40 for i in range(3000)))"
# The model would copy the path from the cut; the script finds the same file the way the worker names it.
READ = "import glob, os\nshow(max(glob.glob(os.environ['XDG_CACHE_HOME'] + '/low-out/run-*'), key=os.path.getmtime), 1501, 1501)"
e = Env([{'code': LONG}, {'code': READ}, {'content': 'read it'}])
r = e.low('--json', 'print a long listing, then read its middle')
outs = e.tools()
first, second = (outs + ['', ''])[:2]
path = first.split('are in ')[-1].split(':')[0]
check('the turn completes', r['code'] == 0 and r['result'].get('answer') == 'read it', r['result'])
check('the cut names a file outside the repo', 'characters omitted; all ' in first and '/low-out/run-' in path
      and not path.startswith(e.repo), first[15990:16200])
check('show() reads the whole output from it', f'[{path}: lines 1501-1501 of 3000]\n1501: line 1500 ' in second, second)
check('no question about stray files', len(outs) == 2, outs[2:])

# Every low in a repo shares the folder: a worker deletes only its own older outputs, and others' once a day old.
PLANT = ("import os, time\nd = os.environ['XDG_CACHE_HOME'] + '/low-out'\nos.makedirs(d, exist_ok=True)\n"
         "for i in range(12):\n    open(f'{d}/run-other-{i}.txt', 'w').write('x')\n"
         "open(f'{d}/run-stale.txt', 'w').write('x')\nos.utime(f'{d}/run-stale.txt', (time.time() - 2 * 86400,) * 2)")
LIST = "import os\nprint(sorted(os.listdir(os.environ['XDG_CACHE_HOME'] + '/low-out')))"
e = Env([{'code': PLANT}, {'code': LONG}, {'code': LIST}, {'content': 'listed'}])
e.low('--json', 'plant, print a long listing, list the folder')
listed = (e.tools() + ['', '', ''])[2]
check("another worker's recent outputs stay", all(f"'run-other-{i}.txt'" in listed for i in range(12)), listed)
check('a day-old one is deleted', "'run-stale.txt'" not in listed, listed)
check("this run's output is there under a name of its own", "'run-2-" in listed, listed)
done()
