"""Through a symlink, edit() changes the file the link points to, and low counts that as the turn's own change: the
report lists the file with no "outside" row and an undo command that brings it back, and undo() in a run restores the
files behind links to a file and to a folder, the links staying links."""
import os
from harness import Env, check, done

UNDO = r'''
import os
edit('link.py', 'x = 1', 'x = 3')
edit('lib/m.py', 'm = 1', 'm = 2')
undo()
print("after", repr(open('real.py').read()), repr(open('pkg/m.py').read()), os.path.islink('link.py'),
      os.path.islink('lib'))
'''
e = Env([{'code': "edit('link.py', 'x = 1', 'x = 2')"}, {'content': 'bumped x'}, {'code': UNDO}, {'content': 'undone'}],
        files={'real.py': 'x = 1\n', 'pkg/m.py': 'm = 1\n'})
os.symlink('real.py', os.path.join(e.repo, 'link.py'))
os.symlink('pkg', os.path.join(e.repo, 'lib'))
e.git('add', '-A')
e.git('commit', '-qm', 'links')
linked = lambda: os.path.islink(os.path.join(e.repo, 'link.py'))

r = e.low('--json', 'bump x through the link')
rows = dict(r['result'].get('rows', []))
check('the edit goes through the link', e.read('real.py') == 'x = 2\n' and linked(), e.read('real.py'))
check("the report counts the file as the turn's own change",
      r['result'].get('changes') == ['real.py +1 −1'] and 'outside' not in rows, rows)
undo = r['result'].get('undo', '').split('  ')[0]
check('its undo command restores the tree', undo.startswith('git restore') and undo.endswith(' -- .'), undo)
os.system(f'cd {e.repo} && {undo} 2>/dev/null')
check('and brings the file back, keeping the link', e.read('real.py') == 'x = 1\n' and linked(), e.read('real.py'))

e.low('--json', 'bump x and m through the links, then undo it')
out = (e.tools() or [''])[-1]
check('undo() in a run restores the files behind both links',
      'undo real.py: ok' in out and 'undo pkg/m.py: ok' in out and "after 'x = 1\\n' 'm = 1\\n' True True" in out, out)
done()
