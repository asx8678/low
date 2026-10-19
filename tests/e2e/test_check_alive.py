"""A check() that passes while processes the run started are still going may rest on them, as on a server the check's
tests reach on localhost (macOS's sandbox lets them): the turn's end runs the check again instead of reusing that pass,
and the verdict comes from it. With nothing left running, the pass is reused as before."""
import socket, sys
from harness import Env, check, done

with socket.socket() as s:
    s.bind(('127.0.0.1', 0))
    PORT = s.getsockname()[1]
REACH = f"""check = '''python3 -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:{PORT}/', timeout=5)"'''\n"""
SERVE = ("import subprocess, time\n"
         f"srv = subprocess.Popen(['python3', '-m', 'http.server', '{PORT}', '--bind', '127.0.0.1'], "
         "stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)\n"
         "time.sleep(1.5)\nwrite('a.txt', 'a\\n')\nprint(check())")


def gates(e):
    return [x['check'] for x in e.log() if isinstance(x.get('check'), dict) and x['check'].get('gate')]


e = Env([{'code': SERVE}, {'content': 'done'}, {'content': 'still'}], low_toml=REACH)
r = e.low('--json', 'write a.txt')
first = (e.tools() + [''])[0]
if sys.platform == 'darwin':
    check('the in-run check passed with the server up', '[check passed' in first, first)
ran = gates(e)
check('the turn end ran the check itself', bool(ran) and not ran[0].get('reused'), ran)
check('and the verdict is that run\'s', r['result'].get('verdict') == 'UNVERIFIED' and r['code'] == 1, r['result'])

e = Env([{'code': "write('a.txt', 'a\\n')\nprint(check())"}, {'content': 'done'}], low_toml='check = "true"\n')
r = e.low('--json', 'write a.txt')
ran = gates(e)
check('with nothing left running, the turn end reuses the pass', bool(ran) and ran[0].get('reused') is True, ran)
check('VERIFIED', r['result'].get('verdict') == 'VERIFIED' and r['code'] == 0, r['result'])
done()
