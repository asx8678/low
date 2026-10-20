"""A run() whose arguments name something other than code (DeepSeek sometimes sends command, as for a shell tool) is
not run: the model is told which argument it sent and what run() takes, not that the output limit cut it off."""
import json, os
from harness import Env, check, done

e = Env([{'raw_args': json.dumps({'command': "touch RAN && grep -n x a.txt"})}, {'code': "print('clean run')"},
         {'content': 'done'}])
r = e.low('--json', 'look at a.txt')
tools = e.tools()
check('the turn completes', r['code'] == 0 and r['result'].get('answer') == 'done', r['result'])
check('the model is told which argument it sent', bool(tools) and 'sent command' in tools[0]
      and 'output limit' not in tools[0], tools)
check('none of it ran', not os.path.exists(os.path.join(e.repo, 'RAN')), tools)
check('the next run runs', len(tools) == 2 and 'clean run' in tools[1], tools)
done()
