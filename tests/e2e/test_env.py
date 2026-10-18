"""The key reaches the provider while neither the model's code nor, on macOS, any process reading low's startup
environment (sysctl KERN_PROCARGS2, which no sandbox rule blocks) can see it: low restarts itself with only the
variables it passes on and takes the key through a pipe."""
import sys
from harness import Env, check, done

KEY, TOKEN = 'sk-pipe-test-0123456789abcdef', 'ghp_shouldnotleak0123456789'
CODE = '''import ctypes, os, sys
print('env:', [k for k, v in os.environ.items() if v in (KEY, TOKEN)])
if sys.platform == 'darwin':
    libc = ctypes.CDLL('/usr/lib/libSystem.B.dylib', use_errno=True)
    mib, size = (ctypes.c_int * 3)(1, 49, os.getppid()), ctypes.c_size_t(1 << 20)
    buf = ctypes.create_string_buffer(size.value)
    if libc.sysctl(mib, 3, buf, ctypes.byref(size), None, 0):
        print('procargs: denied')
    else:
        raw = buf.raw[:size.value]
        print('procargs:', 'low' in raw.decode(errors='replace'), KEY.encode() in raw, TOKEN.encode() in raw)
'''.replace('KEY', repr(KEY)).replace('TOKEN', repr(TOKEN))
e = Env([{'code': CODE}, {'content': 'looked'}])
url = f'http://127.0.0.1:{e.port}/v1'
e.write_home('.config/low/providers.toml', f'[mock]\nbase_url = "{url}"\nmodels = ["mock-model"]\n')
r = e.low('--json', 'look', env={'LOW_API_KEY': KEY, 'GITHUB_TOKEN': TOKEN})
out = (e.tools() or [''])[-1]
check('the key reaches the provider', [x['auth'] for x in e.entries()] == [f'Bearer {KEY}'] * 2, [x['auth'] for x in e.entries()])
check("the model's code has neither in its environment", 'env: []' in out, out)
if sys.platform == 'darwin':
    check("low's startup environment holds neither", 'procargs: True False False' in out, out)
done()
