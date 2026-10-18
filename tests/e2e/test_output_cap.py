"""A run that prints without end stops filling the disk at 64 MB: the rest goes nowhere, the model is told, and the
turn goes on."""
import time
from harness import Env, check, done

CODE = "import time\nt = time.time()\nwhile time.time() - t < 3:\n    print('x' * (1 << 20))\nprint('the end')"
e = Env([{'code': CODE}, {'content': 'printed a lot'}])
began = time.time()
r = e.low('--json', 'print a lot')
out = (e.tools() or [''])[-1]
check('the turn completes', r['code'] == 0 and r['result'].get('answer') == 'printed a lot', r['result'])
check('the model is told the output was dropped', 'printed more than 64 MB; the rest was dropped' in out, out[-300:])
check('and the result stays bounded', len(out) < 40000, len(out))
done()
