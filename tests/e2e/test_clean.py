"""sh() refuses git clean -x or -X, which deletes the files git ignores (an .env, a build cache) that no undo restores;
a dry run, a plain git clean and the words in an echo still run."""
import os
from harness import Env, check, done

FILES = {'.gitignore': '.env.local\n', 'README.md': 'hi\n'}
CODE = ("print(sh('git clean -fdX'))\nprint(sh('FOO=1 git clean -x -f'))\nprint(sh('echo git clean -fdx'))\n"
        "print(sh('git clean -n -x'))\nprint(sh('git clean -fd'))")
e = Env([{'code': CODE}, {'content': 'cleaned'}], files=FILES)
e.write('.env.local', 'SECRET=1\n')
e.write('scratch.txt', 'x\n')
e.low('--json', 'clean up')
out = (e.tools() or [''])[-1]
check('git clean -fdX is refused', out.count('[refused: git clean -x or -X') == 2 and '.env.local' in out, out)
check('the ignored file is kept', os.path.exists(os.path.join(e.repo, '.env.local')), out)
check('echo of the words runs', 'git clean -fdx\n' in out, out)
check('a dry run runs', 'Would remove .env.local' in out, out)
check('a plain git clean runs', not os.path.exists(os.path.join(e.repo, 'scratch.txt')), out)
done()
