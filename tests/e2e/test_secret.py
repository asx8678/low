"""The sandbox hides OpenCode's key file: a fake ~/.local/share/opencode/auth.json in the isolated HOME can't be read
from a run, while a neighbouring file in the same folder can."""
from harness import Env, check, done

CODE = ("import os\nd = os.path.expanduser('~/.local/share/opencode')\n"
        "for f in ('auth.json', 'other.txt'):\n"
        "    try:\n        print(f, open(os.path.join(d, f)).read().strip())\n"
        "    except OSError as e:\n        print(f, type(e).__name__)\n")
e = Env([{'code': CODE}, {'content': 'done'}])
e.write_home('.local/share/opencode/auth.json', '{"fake": "not a real key"}\n')
e.write_home('.local/share/opencode/other.txt', 'visible\n')
e.low('--json', 'look')
out = (e.tools() or [''])[-1]
check('auth.json is hidden', 'auth.json PermissionError' in out and 'not a real key' not in out, out)
check('the rest of the folder stays readable', 'other.txt visible' in out, out)
done()
