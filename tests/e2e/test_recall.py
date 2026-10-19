"""recall(n) after a compaction: the summary says the entries it replaced open with recall(n), and recall() prints one
as it was, from the session log the sandbox hides: the run's code, its result and the reasoning behind it. Entries the
context still holds, and ones that don't exist (a negative too), are refused."""
import re
from harness import Env, check, done

FILLER = 'x' * 1500
FIND = r'''
import contextlib, io
found, refused, first = [], 0, ""
for n in range(1, 400):
    page = io.StringIO()
    with contextlib.redirect_stdout(page):
        recall(n)
    text = page.getvalue()
    if "TOKEN-ALPHA" in text and "its result]" in text:
        found.append(n)
        first = first or text
    refused += text.startswith(f"[#{n}: still in your context")
print("found", found, "refused", refused)
print(first)
recall(100000)
recall(-1)
'''
tasks = [('task01 alpha', 'print("TOKEN-ALPHA")\nprint("' + FILLER + '")')]
tasks += [(f'task{k:02} step', f'print("{k}")\nprint("{FILLER}")') for k in range(2, 9)]
rules = [{'match': t, 'reply': {'reasoning': 'Looking for ' + t, 'code': c}} for t, c in tasks]
rules.append({'match': 'task recall', 'reply': {'code': FIND}})
e = Env({'rules': rules, 'after_tool': {'content': 'Done with that step.'}, 'default': {'content': 'Done.'}},
        profile='compact_at = 3000\n')
codes = [e.low('--json', *(['--resume'] if k else []), t)['code'] for k, (t, _) in enumerate(tasks)]
check('the filler tasks complete', codes == [0] * len(tasks), codes)
check('the session compacted before the recall', any('compact' in x for x in e.log()), [list(x) for x in e.log()])
run = e.low('--json', '--resume', 'task recall')
said = [m.get('content') or '' for b in run['requests'] for m in b['messages']]
check('the summary says how to open what it replaced', any('recall(n) prints entry #n' in s for s in said), said[:3])
out = e.tools()[-1]
check('recall() finds the first run, compacted away, by its token', 'found [' in out and 'found []' not in out, out)
whole = r": the run's code\]\n\d+: print\(\"TOKEN-ALPHA\"\).*: its result\]\n\d+: TOKEN-ALPHA\n.*" \
        r": your reasoning\]\n\d+: Looking for task01 alpha"
check('it prints the run whole, numbered as show() numbers lines: code, result, then the reasoning',
      re.search(whole, out, re.S) and out.splitlines()[1].startswith('[#'), out)
check('what the context still holds is refused, and so is an entry that does not exist',
      'refused 0' not in out and '[#100000: no such entry' in out, out[-400:])
check('a negative entry is refused too, not taken for a check() request', '[#-1: no such entry' in out, out[-400:])
reads = next(x['meta'].get('reads') for x in reversed(e.log()) if x.get('role') == 'tool')
check('the run lists what it recalled by #n, not a temporary file',
      reads and all(r.startswith('#') for r in reads), reads)
done()
