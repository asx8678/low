"""A run whose output is cut keeps its whole output in a file outside the tree, inside the sandbox: the cut names it,
the next run reads the middle with show(), and the repo stays as it was, so no stray-file question follows."""
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
done()
