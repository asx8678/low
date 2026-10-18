"""low's own rewrite of the --tasks checklist is not reported to the model as a change made outside the conversation;
a real outside change between runs still is."""
from harness import Env, check, done, user_messages

e = Env([{'content': 'first done'}, {'content': 'second done'}],
        files={'README.md': 'hi\n', 'TASKS.md': '# Checklist\n\n- [ ] first item\n- [ ] second item\n'})
r = e.low('--json', '--tasks', 'TASKS.md')
second = user_messages(r['requests'][1]) if len(r['requests']) > 1 else []
moved = [m for m in second if 'Files changed since your last turn' in m]
check("second item: no 'files changed' note about the checklist", not any('TASKS.md' in m for m in moved), moved)
check('both items done', r['result'].get('left') == 0, r['result'])

e = Env([{'content': 'first done'}, {'content': 'second done'}], files={'README.md': 'hi\n', 'TASKS.md': '- [ ] first item\n'})
e.low('--json', '--tasks', 'TASKS.md')
e.write('notes.txt', 'outside edit\n')
with open(f'{e.repo}/TASKS.md', 'a') as f:
    f.write('- [ ] second item\n')
r = e.low('--json', '--tasks', 'TASKS.md')
first = user_messages(r['requests'][0]) if r['requests'] else []
moved = [m for m in first if 'Files changed since your last turn' in m]
check('an outside change between runs is still reported', any('notes.txt' in m for m in moved), first)
check('without the checklist in it', not any('TASKS.md' in m for m in moved), moved)
done()
