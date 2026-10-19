"""A folder's AGENTS.md goes to the model the first time a run shows, edits or writes a file under it, and not again
while the context still holds it: in the next run, and in the next task of a resumed session. The root's stays in the
system prompt only."""
from harness import Env, check, done

FILES = {'AGENTS.md': 'ROOT-RULES\n', 'pkg/AGENTS.md': 'PKG-RULES: run make pkg\n', 'pkg/m.py': 'a = 1\n'}
e = Env([{'code': "show('pkg/m.py')"}, {'code': "edit('pkg/m.py', 'a = 1', 'a = 2')\nshow('pkg/m.py')"},
         {'content': 'done'}, {'code': "write('pkg/n.py', 'b = 1\\n')"}, {'content': 'done again'}], files=FILES)
r = e.low('--json', 'change a')
tools = e.tools()
check('the first run gets the folder\'s instructions', len(tools) >= 1 and
      '[pkg/AGENTS.md: follow it for the files in pkg/; shown once]\nPKG-RULES: run make pkg\n' in tools[0], tools)
check('the next run doesn\'t get them again', len(tools) >= 2 and 'PKG-RULES' not in tools[1], tools)
check('the root\'s are in the system prompt, not a run', 'ROOT-RULES' in r['requests'][0]['messages'][0]['content']
      and not any('ROOT-RULES' in t for t in tools), '')
r = e.low('--json', '--resume', 'add n.py')
tools = e.tools()
check('a resumed session remembers them', r['code'] == 0 and len(tools) == 3 and 'PKG-RULES' not in tools[2]
      and 'write pkg/n.py: ok' in tools[2], tools[2:])
done()
