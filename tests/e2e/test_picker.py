"""/models in a real terminal: the arrows pick the model and the effort in one panel, Enter applies them as the session's
switch, Esc and Ctrl-C change nothing, and the terminal gets its cursor back. Without a terminal it is the numbered
list."""
import fcntl, json, os, pty, select, struct, subprocess, termios, time
from harness import LOW, Env, check, done

UP, DOWN, RIGHT, LEFT, ESC = '\x1b[A', '\x1b[B', '\x1b[C', '\x1b[D', '\x1b'
e = Env([], profile='reasoning_effort = "low"')


def switches():
    return [x['switch'] for x in e.log() if 'switch' in x]


class Repl:
    def __init__(self, *args):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack('HHHH', 30, 100, 0, 0))
            os.chdir(e.repo)
            os.execve(LOW, [LOW, *args], e.env({'TERM': 'xterm-256color'}))
        self.out = ''

    def until(self, text, timeout=15, start=None):
        """Reads the terminal until `text` shows up after `start` (what was read before); returns what came since."""
        start, end = len(self.out) if start is None else start, time.time() + timeout
        while text not in self.out[start:] and time.time() < end:
            if select.select([self.fd], [], [], 0.2)[0]:
                try:
                    self.out += os.read(self.fd, 65536).decode(errors='replace')
                except OSError:
                    break
        return self.out[start:]

    def send(self, keys, wait=None):
        """Types the keys one at a time, reading what low draws in between, as a terminal does: a pty that nobody reads
        fills up and stalls low mid-draw, and keys then pile up and split."""
        start = len(self.out)
        for key in keys:
            os.write(self.fd, key.encode())
            self.until('\x00', timeout=0.15)
        return self.until(wait, start=start) if wait else self.out[start:]

    def close(self):
        os.write(self.fd, b'\x04')
        self.until('\x00', timeout=5)
        return os.waitpid(self.pid, 0)[1]


r = Repl()
r.until('› ')
shown = r.send(['/models\r'], wait='esc cancel')
check('the panel lists both providers\' models and marks the current one',
      'mock-model' in shown and 'spare-model' in shown and '● current' in shown, shown[-600:])
check("the default effort names the profile's", 'default (low)' in shown, shown[-600:])
r.send([DOWN, RIGHT, RIGHT, RIGHT, '\r'], wait='reasoning effort high')
time.sleep(0.3)
last = switches()[-1]
check('Enter switches to the model and effort picked',
      (last['provider'], last['model'], last['params'].get('reasoning_effort')) == ('spare', 'spare-model', 'high'), last)
check('a model and an effort make two switches', len(switches()) == 2, len(switches()))

count = len(switches())
r.send(['/models\r'], wait='esc cancel')
r.send([UP, LEFT, ESC])
r.send(['/models\r'], wait='esc cancel')
r.send([UP, '\x03'])
r.send(['/cost\r'], wait='› ')
check('Esc and Ctrl-C change nothing', len(switches()) == count, switches()[count:])

r.send(['/models\r'], wait='esc cancel')
r.send([UP] + [LEFT] * 6 + ['\r'], wait='now using mock-model')
time.sleep(0.3)
last = switches()[-1]
check('back to the default effort takes the profile again',
      (last['model'], last['params'].get('reasoning_effort')) == ('mock-model', 'low'), last)

r.send(['/models spare\r'], wait='esc cancel')
r.send(['\r'], wait='now using spare-model')
check('/models <filter> starts filtered', switches()[-1]['model'] == 'spare-model', switches()[-1])

r.send(['/models\r'], wait='esc cancel')
r.send(['zzz-new', '\r'], wait='not listed')
check('a typed unlisted model is tried on the current provider',
      (switches()[-1]['provider'], switches()[-1]['model']) == ('spare', 'zzz-new'), switches()[-1])
code = r.close()
check('the cursor is shown again each time it was hidden', r.out.count('\x1b[?25l') == r.out.count('\x1b[?25h') == 6,
      (r.out.count('\x1b[?25l'), r.out.count('\x1b[?25h')))
check('the REPL ends normally', code == 0, code)

before = len(e.sessions())
p = subprocess.run([LOW], cwd=e.repo, env=e.env(), input='/models\n2\n', capture_output=True, text=True, timeout=60)
log = [json.loads(l) for l in open(e.sessions()[-1])]
picked = [x['switch']['model'] for x in log if 'switch' in x]
check('without a terminal it is the numbered list', picked == ['spare-model'] and '\x1b[?25l' not in p.stdout,
      (picked, p.stdout[-300:], p.stderr[-300:]))
done()
