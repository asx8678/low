"""An active virtualenv's python runs the model's code, so the project's packages import."""
import os, subprocess, sys
from harness import Env, check, done

e = Env([{'code': 'import sys\nprint(sys.prefix)'}, {'content': 'done'}])
venv = os.path.join(e.dir, 'venv')
subprocess.run([sys.executable, '-m', 'venv', '--without-pip', venv], check=True)
r = e.low('--json', 'which python', env={'VIRTUAL_ENV': venv, 'PATH': f'{venv}/bin:{os.environ["PATH"]}'})
out = (e.tools() or [''])[-1].strip()
check('the worker runs in the venv', out == venv, out or r['err'][-300:])
done()
