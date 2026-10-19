"""The repository's instructions go in the system prompt: AGENTS.md, else CLAUDE.md, never both."""
from harness import Env, check, done


def system(files):
    e = Env([{'content': 'ok'}], files=files)
    r = e.low('--json', 'say ok')
    return r['requests'][0]['messages'][0]['content'] if r['requests'] else ''


s = system({'CLAUDE.md': 'Run the tests with make test. MARK-CLAUDE\n'})
check('CLAUDE.md goes in when there is no AGENTS.md', '# Project instructions (CLAUDE.md)' in s and 'MARK-CLAUDE' in s,
      s[-400:])
s = system({'AGENTS.md': 'MARK-AGENTS\n', 'CLAUDE.md': 'MARK-CLAUDE\n'})
check('AGENTS.md goes in alone when there are both', '# Project instructions (AGENTS.md)' in s and 'MARK-AGENTS' in s
      and 'MARK-CLAUDE' not in s, s[-400:])
done()
