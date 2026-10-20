"""A turn that changes files with no check passing gets a second look: the same model, in a conversation of its own that
has the system prompt and a brief (task, criteria, what the runs did, the diff, the report) but none of the turn's
messages, reads the code in a worker that can't change files and gives each claim it checked a verdict. What it finds
contradicted goes back to the model once, and a second look at the answer checks those claims alone, with the runs
and the diff since; the report and --json carry the first look's other claims and what the second made of those. A
look's last request is told to give its verdict. A passing check, or a turn without changes, gets none."""
from harness import Env, check, done, user_messages

LOOK = 'You are taking a second look'
FILES = {'app.py': "def greet():\n    return 'hi'\n\n\ndef bye():\n    return 'bye'\n"}
EDITED = "def greet():\n    return 'hello'\n\n\ndef bye():\n    return 'bye'\n"
FINAL = ("accept('greet() says hello; bye() is unchanged')\nedit('app.py', \"return 'hi'\", \"return 'hello'\")\n"
         "finish(\"greet() now returns 'hello'; bye() is unchanged\")")
READS = ("try:\n    write('app.py', 'x')\nexcept ValueError as e:\n    print('refused:', e)\n"
         "try:\n    open('app.py', 'a').write('x')\nexcept OSError as e:\n    print('denied:', type(e).__name__)\n"
         "import os, tempfile\nopen(os.path.join(tempfile.gettempdir(), 'look.txt'), 'w').write('ok')\nprint('tmp ok')\n"
         "print('check:', check())\nshow('app.py')")
e = Env({'replies': [{'code': FINAL}, {'content': "The reader is wrong: bye() still returns 'bye' (app.py:6)."}],
         'asides': [{'code': READS},
                    {'code': "finish(\"held: greet() returns 'hello' — app.py:2\\n"
                             "contradicted: bye() is unchanged — app.py:6 returns 'bye!'\")"},
                    {'code': "finish(\"- **held**: bye() still returns 'bye' — app.py:6\")"}]}, files=FILES)
r = e.low('--json', 'make greet say hello')
res, asides, log = r['result'], r['asides'], e.log()
check('second look: two passes of the model, three of the second look',
      (len(r['requests']), len(asides)) == (2, 3), (len(r['requests']), len(asides)))
check('second look: verdict NO CHECK, exit 0, the first look\'s held claim and the second\'s',
      res.get('verdict') == 'NO CHECK' and r['code'] == 0
      and res.get('claims') == {'held': ["greet() returns 'hello' — app.py:2", "bye() still returns 'bye' — app.py:6"],
                                'contradicted': [], 'uncertain': []},
      res)
first = asides[0]['messages']
brief = first[1]['content'] if len(first) == 2 else ''
check('second look: the same system prompt, then only the brief',
      [m['role'] for m in first] == ['system', 'user'] and first[0] == r['requests'][0]['messages'][0], first[1:])
check('second look: the brief has the task, criteria, runs, diff and report',
      brief.startswith(LOOK) and 'make greet say hello' in brief and 'greet() says hello; bye() is unchanged' in brief
      and 'run 1: edit app.py' in brief and "+    return 'hello'" in brief and "-    return 'hi'" in brief
      and "## Report\ngreet() now returns 'hello'; bye() is unchanged" in brief, brief)
ran = asides[1]['messages'][-1]['content']
check('second look: edit()/write() refuse, the sandbox denies writes in the repo, temp dirs stay writable',
      'refused: app.py stays as it is' in ran and 'denied: ' in ran and 'tmp ok' in ran, ran)
check('second look: check() is turned down, show() reads',
      'not available in a second look' in ran and 'check: False' in ran and "2:     return 'hello'" in ran, ran)
check('second look: the repo is as the turn left it', e.read('app.py') == EDITED, e.read('app.py'))
sent = user_messages(r['requests'][1])
check('second look: the contradicted claim goes back to the model, as low',
      'found claims the code contradicts' in sent[-1] and "bye() is unchanged — app.py:6 returns 'bye!'" in sent[-1],
      sent)
main = ''.join(str(m.get('content')) for body in r['requests'] for m in body['messages'])
check('second look: none of it reaches the turn\'s conversation', LOOK not in main and 'tmp ok' not in main)
again = asides[2]['messages'][1]['content']
check('second look: the answer gets a second look at the contradicted claim alone, with nothing changed since',
      again.startswith(LOOK) and 'Check only those claims' in again
      and "## Claims the earlier look found contradicted\n- bye() is unchanged — app.py:6 returns 'bye!'" in again
      and '## Diff since\n[no change since the earlier look]' in again and "+    return 'hello'" not in again
      and "## Answer\nThe reader is wrong: bye() still returns 'bye' (app.py:6)." in again, again)
logged = [x for x in log if x.get('event') == 'aside']
check('second look: logged as aside events, with usage and no role',
      len(logged) == 3 and all(x['meta'].get('aside') and x['meta'].get('usage') and 'role' not in x for x in logged)
      and not any(LOOK in str(x.get('content')) for x in log if x.get('role')), logged)
check('second look: its requests count in the turn\'s', res.get('requests') == 5, res.get('requests'))
report = [x['report'] for x in log if 'report' in x][-1]
check('second look: the log\'s report has the claims', report.get('claims') == res.get('claims'), report)
check('second look: the report has a claims row',
      ['claims', '2 held, by a second look at the diff'] in res.get('rows', []), res.get('rows'))

# The model's answer changes the code: the second look at it gets the runs and the diff since the first look, and its
# last request (the third) is told to give its verdict.
ANSWER = "edit('app.py', \"return 'hello'\", \"return 'hello!'\")\nfinish(\"greet() now returns 'hello!'\")"
e = Env({'replies': [{'code': FINAL}, {'code': ANSWER}],
         'asides': [{'code': "finish(\"held: bye() is unchanged — app.py:6\\n"
                             "contradicted: greet() returns 'hello!' — app.py:2 returns 'hello'\")"},
                    {'code': "show('app.py')"}, {'code': "show('app.py')"},
                    {'code': "finish(\"held: greet() returns 'hello!' — app.py:2\")"}]}, files=FILES)
r = e.low('--json', 'make greet say hello')
res, asides = r['result'], r['asides']
again = asides[1]['messages'][1]['content'] if len(asides) > 1 else ''
check('recheck: one request of the first look, three of the second', len(asides) == 4, len(asides))
check('recheck: the runs and the diff since the first look',
      "## What the turn's runs did since" in again and 'run 2: edit app.py' in again and 'run 1:' not in again
      and "+    return 'hello!'" in again and "-    return 'hi'" not in again
      and "## Answer\ngreet() now returns 'hello!'" in again, again)
last = [m for a in asides for m in a['messages'][-1:]]
check('recheck: only its last request is told to give its verdict',
      [m['role'] == 'user' and m['content'].startswith('That was your last run') for m in last]
      == [False, False, False, True], last)
check('recheck: the claims as it left them', res.get('claims') == {
    'held': ['bye() is unchanged — app.py:6', "greet() returns 'hello!' — app.py:2"], 'contradicted': [],
    'uncertain': []} and r['code'] == 0, res)

# A first look that keeps reading is told on its sixth request to give its verdict.
e = Env({'replies': [{'code': FINAL}],
         'asides': [{'code': "show('app.py')"}] * 5 + [{'code': "finish(\"held: greet() returns 'hello' — app.py:2\")"}]},
        files=FILES)
r = e.low('--json', 'make greet say hello')
last = [a['messages'][-1] for a in r['asides']]
check('last request: the sixth is told to give its verdict, and gives it',
      len(last) == 6 and [m['role'] == 'user' and m['content'].startswith('That was your last run') for m in last]
      == [False] * 5 + [True] and r['result'].get('claims', {}).get('held') == ["greet() returns 'hello' — app.py:2"],
      (last, r['result'].get('claims')))

# The mock's own answer to a second look leaves a claim uncertain: nothing goes back, the turn ends.
e = Env([{'code': FINAL}], files=FILES)
r = e.low('--json', 'make greet say hello')
check('uncertain: nothing goes back', (len(r['requests']), len(r['asides'])) == (1, 1)
      and r['result'].get('claims', {}).get('uncertain') and r['code'] == 0, r['result'])

# A check that passes, or a turn that changes nothing, gets no second look.
e = Env([{'code': FINAL.replace('finish(', 'assert check()\nfinish(')}], files=FILES, low_toml='check = "python3 -c pass"\n')
r = e.low('--json', 'make greet say hello')
check('passing check: no second look', r['result'].get('verdict') == 'VERIFIED' and not r['asides']
      and r['result'].get('claims') is None, r['result'])
e = Env([{'code': "show('app.py')"}, {'content': "greet() returns 'hi' (app.py:2)."}], files=FILES)
r = e.low('--json', 'what does greet return?')
check('no changes: no second look', r['result'].get('verdict') == 'NO CHANGES' and not r['asides'], r['result'])
done()
