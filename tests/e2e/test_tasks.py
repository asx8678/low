"""--tasks sends the checklist's preamble once with the first task, continues a task that a turn limit cut off while it
was still changing files, stops one that made no progress, caps the result line's file list, and keeps the checklist
low's: the model's edit() of it is refused while low's own marks land."""
import re
from harness import Env, check, done, user_messages

PROFILE = 'compact_at = 1024\n'  # turn ceiling = 60 x 1024 tokens
BIG = {'prompt': 30000, 'cached': 0}  # two such requests nearly exhaust the turn

e = Env([{'content': 'one'}, {'content': 'two'}],
        files={'TASKS.md': '# Port checklist\n\nRules: keep it short.\n\n- [ ] first item\n- [ ] second item\n'})
r = e.low('--json', '--tasks', 'TASKS.md')
first = user_messages(r['requests'][0])[-1] if r['requests'] else ''
second = user_messages(r['requests'][1])[-1] if len(r['requests']) > 1 else ''
check('first task carries the preamble',
      first.startswith('first item') and 'Rules: keep it short.' in first and 'TASKS.md' not in first, first)
check('second task does not repeat it', 'Rules' not in second, second)

TASKS = '- [ ] build the thing\n- [ ] then this\n'
e = Env([{'code': "write('a.txt', 'a\\n')", **BIG}, {'code': "write('b.txt', 'b\\n')", **BIG},
         {'code': "write('c.txt', 'c\\n')", **BIG}, {'content': 'built'}, {'content': 'this done'}],
        profile=PROFILE, files={'TASKS.md': TASKS})
r = e.low('--json', '--tasks', 'TASKS.md')
tasks = [t.get('task') for t in r['result'].get('tasks', [])]
check('a limited task that changed files runs again', tasks.count('build the thing') == 2, tasks)
check('and every task is done', r['result'].get('left') == 0 and r['code'] == 0, r['result'])
check('the checklist marks both done', e.read('TASKS.md').count('- [x]') == 2, e.read('TASKS.md'))

e = Env([{'code': 'print(1)', **BIG}, {'code': 'print(2)', **BIG}, {'code': 'print(3)', **BIG}, {'content': 'never'}],
        profile=PROFILE, files={'TASKS.md': TASKS})
r = e.low('--json', '--tasks', 'TASKS.md')
check('a no-progress task stops after one turn', len(r['result'].get('tasks', [])) == 1 and r['code'] == 5, r['result'])

e = Env([{'code': "for i in range(12): write(f'f{i}.txt', 'x\\n')"}, {'content': 'twelve'}],
        files={'TASKS.md': '- [ ] make twelve\n'})
e.low('--json', '--tasks', 'TASKS.md')
line = next((l for l in e.read('TASKS.md').splitlines() if '- low:' in l), '')
check('the result line names 8 files and counts the rest',
      len(re.findall(r'f\d+\.txt', line)) == 8 and 'and 4 more' in line, line)

e = Env([{'code': "edit('TASKS.md', '- [ ] tick me', '- [x] tick me')"}, {'content': 'tried'}],
        files={'TASKS.md': '- [ ] tick me\n'})
e.low('--json', '--tasks', 'TASKS.md')
body = e.read('TASKS.md')
check("the model's edit of the checklist is refused", bool(e.tools()) and 'protected' in e.tools()[-1], e.tools())
check('low still marks the task', '- [x] tick me' in body or '- [!] tick me' in body, body)
done()
