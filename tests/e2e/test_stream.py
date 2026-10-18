"""A reply counts as whole only with a finish reason or [DONE]: a cut stream is retried from the start, also the first
reply of a --resume process; DeepSeek's broken-off finish reasons are retried; a call cut by the output limit never
runs; split UTF-8, CRLF line ends and keep-alive comments parse."""
import json, os
from harness import Env, check, done

CUT = {'content': 'This reply will be cut off in the middle of a sentence and never fin', 'cut_after': 700,
       'no_done': True, 'finish': 'none'}
cases = {
    'a cut stream': [CUT, {'content': 'Second try, whole.'}],
    'insufficient_system_resource': [{'content': 'The answer is that add() sub', 'finish': 'insufficient_system_resource'},
                                     {'content': 'Second try, whole.'}],
    'aborted': [{'content': 'Partial answ', 'finish': 'aborted'}, {'content': 'Second try, whole.'}],
    'an error event': [{'content': 'Starting...', 'error_event': {'message': 'upstream overloaded', 'code': 503},
                        'finish': 'none', 'no_done': True}, {'content': 'Second try, whole.'}],
}
for name, script in cases.items():
    e = Env(script)
    r = e.low('--json', 'explain it')
    replies = [x.get('content') for x in e.log() if x.get('role') == 'assistant']
    check(f'{name} is retried', r['code'] == 0 and r['result'].get('answer') == 'Second try, whole.'
          and len(r['requests']) == 2, r['result'])
    check(f'{name}: only the whole reply is kept', replies == ['Second try, whole.'], replies)

# The first reply of a resumed process: the old build took the cut text as the answer.
e = Env([{'content': 'First task answer.'}, CUT, {'content': 'Whole second answer.'}])
e.low('--json', 'first')
r = e.low('--json', '--resume', 'second')
check('a cut first reply after --resume is retried', r['result'].get('answer') == 'Whole second answer.', r['result'])

MARK = 'TRUNCATED_RAN'
e = Env([{'raw_args': json.dumps({'code': f'open("{MARK}", "w").write("x")\nprint("hello'})[:-6], 'finish': 'length'},
         {'content': 'Stopping here.'}])
r = e.low('--json', 'explain it')
check('a call cut by the output limit does not run', not os.path.exists(os.path.join(e.repo, MARK)), e.tools())
check('and the model is told so', any('cut off by the output limit' in t for t in e.tools()), e.tools())

text = 'Ünïcødé ✓ 漢字テスト 🧪🔥 done.'
e = Env([{'content': text, 'chunk': 3, 'crlf': True, 'comments': True}])
r = e.low('--json', 'say it')
check('split UTF-8, CRLF and comments parse', r['result'].get('answer') == text, r['result'])
done()
