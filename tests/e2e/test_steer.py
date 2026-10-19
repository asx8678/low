"""Steering from the REPL: what is typed while a turn runs shows above the bar, and Enter sends it with the model's next
request, after the run in flight has its result. What the turn doesn't take runs when it ends: a line sent during the
final reply as the next task, a command after it. A line not sent starts the next prompt, and after Ctrl-C, which still
interrupts, nothing sent runs on its own."""
import fcntl, os, pty, re, select, struct, termios, time
from harness import LOW, Env, check, done

SLEEP = "import time\ntime.sleep({})\nprint('slept')"
e = Env([
    {'code': SLEEP.format(4)}, {'content': 'done first'},  # first, steered during the run, then /cost
    {'content': 'final', 'delay': 3}, {'content': 'ok next'},  # second; next thing, sent during its final reply
    {'code': SLEEP.format(3)}, {'content': 'done third'}, {'content': 'ok half'},  # third; half done, typed unsent
    {'code': SLEEP.format(30)}, {'content': 'ok redo'},  # fourth, interrupted; redo it, sent before the Ctrl-C
])


class Repl:
    def __init__(self):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack('HHHH', 30, 100, 0, 0))
            os.chdir(e.repo)
            os.execve(LOW, [LOW], e.env({'TERM': 'xterm-256color'}))
        self.out = ''

    def until(self, text, timeout=20, start=None):
        """Reads the terminal until `text` shows up after `start`; returns what came since."""
        start, end = len(self.out) if start is None else start, time.time() + timeout
        while text not in self.out[start:] and time.time() < end:
            if select.select([self.fd], [], [], 0.2)[0]:
                try:
                    self.out += os.read(self.fd, 65536).decode(errors='replace')
                except OSError:
                    break
        return self.out[start:]

    def send(self, keys, wait=None):
        start = len(self.out)
        os.write(self.fd, keys.encode())
        return self.until(wait, start=start) if wait else self.until('\x00', timeout=0.3)


def requests(n, timeout=20):
    """Waits for the mock to have seen n requests."""
    end = time.time() + timeout
    while len(e.entries()) < n and time.time() < end:
        time.sleep(0.05)
    return e.entries()


def plain(text):
    return re.sub(r'\x1b(\[[0-9;?]*[A-Za-z]|[78])', '', text)


def users():
    return [(x['content'], x.get('meta', {}).get('steer')) for x in e.log() if x.get('role') == 'user']


def prompt(since, text='› '):
    """What the REPL shows from the end of the turn after `since`, when the bar lets go of the scroll region, until its
    prompt shows `text`: keys sent before then would go to the turn."""
    r.until('\x1b[r', start=since)
    return r.until(text, start=r.out.find('\x1b[r', since))


r = Repl()
r.until('› ')
r.send('first\r', wait='step 1 running')
shown = r.send('use tabs\r', wait='queued: use tabs')
shown += r.send('/cost\r', wait='/cost runs when the turn ends')
check('the row above the bar says how to steer, then what is queued',
      'type to steer the model' in r.out and 'queued: use tabs' in shown and '/cost runs' in shown, shown[-800:])
after = plain(r.until('requests · in'))
sent = requests(2)[1]['body']['messages']
check('the steer goes with the next request, after the run\'s result',
      [m['role'] for m in sent[-2:]] == ['tool', 'user'] and sent[-1]['content'] == '[Steered by the user]\nuse tabs', sent[-3:])
check('it shows where the model got it, and the command runs when the turn ends',
      '› use tabs' in plain(r.out) and 'requests · in' in after, after[-600:])
check('a steer is logged as one, and is no new task', ('use tabs', True) in users(), users())

r.until('› ', start=r.out.rindex('requests · in'))
r.send('second\r')
requests(3)
r.send('next thing\r', wait='ok next')
check('a line sent during the final reply runs as the next task, after the report',
      ('next thing', None) in users() and len(requests(4)) == 4 and 'final' in r.out, users())

prompt(r.out.rindex('ok next'))
r.send('third\r', wait='step 1 running')
r.send('half')
shown = prompt(r.out.rindex('step 1 running'), 'half')
check('a line typed but not sent starts the next prompt', '› half' in plain(shown), shown[-300:])
r.send(' done\r', wait='ok half')
check('it is sent from there', ('half done', None) in users(), users())

prompt(r.out.rindex('ok half'))
r.send('fourth\r', wait='step 1 running')
r.send('redo it\r', wait='queued: redo it')
stopped = r.send('\x03', wait='interrupted')
check('Ctrl-C still interrupts the turn', 'interrupted' in stopped, stopped[-600:])
shown = prompt(r.out.rindex('interrupted'), 'redo it')
time.sleep(1)
check('after Ctrl-C what was sent waits at the prompt instead of running',
      '› redo it' in plain(shown) and len(e.entries()) == 8 and ('redo it', None) not in users(),
      (shown[-300:], len(e.entries()), users()))
r.send('\r', wait='ok redo')
check('Enter sends it then', ('redo it', None) in users(), users())

prompt(r.out.rindex('ok redo'))
os.write(r.fd, b'\x04')
r.until('\x00', timeout=5)
code = os.waitpid(r.pid, 0)[1]
check('the REPL ends normally', code == 0, code)
done()
