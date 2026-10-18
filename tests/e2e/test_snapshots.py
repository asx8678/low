"""The snapshot an undo restores outlives git gc: each turn's starting tree is kept in one commit at
refs/low/snapshots (no reflog, no history), each entry for 14 days, the same tree once."""
import os, time
from harness import Env, check, done

e = Env([{'code': "edit('README.md', 'draft', 'final')"}, {'content': 'edited'}, {'content': 'b'}, {'content': 'c'}])
e.write('README.md', 'draft\n')  # uncommitted, so only the snapshot holds it
r = e.low('--json', 'finalize the readme')
undo = r['result'].get('undo', '').split('  ')[0]
check('the turn edited the file', e.read('README.md') == 'final\n' and undo.startswith('git restore'), r['result'])
e.git('gc', '-q', '--prune=now')
os.system(f'cd {e.repo} && {undo} 2>/dev/null')
check('the undo works after git gc --prune=now', e.read('README.md') == 'draft\n', e.read('README.md'))
check('one commit beyond the user\'s', e.git('rev-list', '--all', '--count') == '2', e.git('log', '--all', '--oneline'))
check('no reflog for the ref', not os.path.exists(os.path.join(e.repo, '.git/logs/refs/low')))

# An entry past 14 days goes; the same tree is kept once.
empty = e.git('hash-object', '-t', 'tree', '/dev/null')
old = int(time.time()) - 15 * 86400
planted = e.git('mktree', input=f'040000 tree {empty}\t{old}-{empty}\n')
e.git('update-ref', 'refs/low/snapshots', e.git('commit-tree', planted, '-m', 'old'))
e.low('--json', 'one')
e.low('--json', 'two')
names = [l.split('\t')[1] for l in e.git('ls-tree', 'refs/low/snapshots').splitlines()]
check('an entry older than 14 days is dropped', not any(n.startswith(f'{old}-') for n in names), names)
check('the same tree is kept once', len(names) == 1, names)
done()
