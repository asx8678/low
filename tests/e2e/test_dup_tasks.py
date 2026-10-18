"""--tasks with two identical open task lines: each runs once and gets its own mark, and the run ends. The same with
an edit made during the second task that moves its line onto the first, finished one."""
from harness import Env, check, done

TASKS = '- [ ] say hi\n- [ ] say hi\n- [ ] last one\n'
# Every reply is plain text, so each task is one request; the old loop kept asking until killed.
e = Env([{'content': 'hi'}] * 12, files={'TASKS.md': TASKS})
r = e.low('--json', '--tasks', 'TASKS.md')
ran, body = [t.get('task') for t in r['result'].get('tasks', [])], e.read('TASKS.md')
check('each task line runs once', ran == ['say hi', 'say hi', 'last one'], f'ran {ran}; {len(r["requests"])} requests')
check('all three lines are marked done', body.count('- [x]') == 3 and r['result'].get('left') == 0, body)

# While the second "say hi" runs (it was on line 3), two lines appear above, so line 3 now holds the finished one.
MOVED = ('<!-- low session: x -->\n# added while low ran\n# and this\n- [x] say hi\n  - low: earlier\n'
         '- [ ] say hi\n- [ ] last one\n')
e = Env([{'content': 'hi'}, {'content': 'hi', 'touch': ['TASKS.md', MOVED]}] + [{'content': 'hi'}] * 10,
        files={'TASKS.md': TASKS})
r = e.low('--json', '--tasks', 'TASKS.md')
ran, body = [t.get('task') for t in r['result'].get('tasks', [])], e.read('TASKS.md')
check('moved: each task line runs once', ran == ['say hi', 'say hi', 'last one'], f'ran {ran}; {len(r["requests"])} requests')
check('moved: the open duplicate gets the mark, the edit stays',
      body.count('- [x]') == 3 and '# and this' in body and body.count('- low:') == 3, body)
done()
