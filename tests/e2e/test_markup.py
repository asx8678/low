"""A run() whose code carries DeepSeek's own tool-call markup is not run: the model is told to send the Python alone,
and the turn goes on with its next reply."""
import os
from harness import Env, check, done

LEAKED = "write('a.txt', 'x\\n')\n<｜end▁of▁thinking｜>\n\n<｜｜DSML｜｜ invoke name=\"run\">"
e = Env([{'code': LEAKED}, {'code': "print('clean run')"}, {'content': 'done'}])
r = e.low('--json', 'write a file')
tools = e.tools()
check('the turn completes', r['code'] == 0 and r['result'].get('answer') == 'done', r['result'])
check('the model is told what was wrong', bool(tools) and 'tool-call markup' in tools[0], tools)
check('none of the code ran', not os.path.exists(os.path.join(e.repo, 'a.txt')), tools)
check('the next run runs', len(tools) == 2 and 'clean run' in tools[1], tools)
done()
