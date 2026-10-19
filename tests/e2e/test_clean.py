"""sh() refuses git clean -x or -X and rm -r of the whole tree, which delete the files git ignores (an .env, a build
cache) that no undo restores; a dry run, a plain git clean, an rm -r of one folder and the words in an echo still run.
subprocess in a run is refused alike, and its commands are logged with sh()'s."""
import os
from harness import Env, check, done

FILES = {'.gitignore': '.env.local\n', 'README.md': 'hi\n'}
CODE = ("print(sh('rm -rf build && echo removed'))\nprint(sh('git clean -fdX'))\nprint(sh('FOO=1 git clean -x -f'))\n"
        "print(sh('echo git clean -fdx'))\nprint(sh('git clean -n -x'))\nprint(sh('git clean -fd'))\nprint(sh('rm -rf ./* ./.[!.]*'))\n"
        "print(sh('rm -r -f \"$PWD\"/'))\nimport subprocess\ntry:\n    subprocess.run('git clean -fdX', shell=True)\n"
        "except PermissionError as e:\n    print(e)\nsubprocess.run(['git', 'status', '--short'])")
e = Env([{'code': CODE}, {'content': 'cleaned'}], files=FILES)
e.write('.env.local', 'SECRET=1\n')
e.write('scratch.txt', 'x\n')
e.write('build/out.o', 'x\n')
e.low('--json', 'clean up')
out = (e.tools() or [''])[-1]
check('git clean -fdX is refused', out.count('[refused: git clean -x or -X') == 3 and '.env.local' in out, out)
cmds = [m['meta'].get('cmds') or [] for m in e.log() if m.get('role') == 'tool']
check('subprocess commands are logged with sh()\'s', [c[:2] for c in cmds[-1][-2:]] ==
      [['git clean -fdX', 'refused'], ['git status --short', 0]], cmds)
check('the ignored file is kept', os.path.exists(os.path.join(e.repo, '.env.local')), out)
check('echo of the words runs', 'git clean -fdx\n' in out, out)
check('a dry run runs', 'Would remove .env.local' in out, out)
check('rm -rf of the whole tree is refused', out.count('[refused: rm -r ') == 2, out)
check('rm -r of one folder runs', not os.path.exists(os.path.join(e.repo, 'build')) and 'removed' in out, out)
check('a plain git clean runs', not os.path.exists(os.path.join(e.repo, 'scratch.txt')), out)
done()
