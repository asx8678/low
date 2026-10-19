"""A paste while a turn runs: the terminal marks it while the steering row is up, so a block of lines, indented, with a
blank line and a line that reads as a command, goes to the model as one steer when Enter sends it, even when it comes in
two pieces. The row shows its line breaks as ⏎, and the terminal stops marking pastes when the turn ends."""
import fcntl, os, pty, re, select, struct, termios, time
from harness import LOW, Env, check, done

e = Env([{'code': "import time\ntime.sleep(4)\nprint('slept')"}, {'content': 'done first'}])


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


def plain(text):
    return re.sub(r'\x1b(\[[0-9;?]*[A-Za-z]|[78])', '', text)


def users():
    return [(x['content'], x.get('meta', {}).get('steer')) for x in e.log() if x.get('role') == 'user']


PASTED = 'fix this:\n    indented\n\n/cost\n'
r = Repl()
r.until('› ')
shown = r.send('first\r', wait='step 1 running')
check('the turn has the terminal mark pastes', '\x1b[?2004h' in shown, shown[-400:])
# A terminal sends a paste's line breaks as Enter would; the second half comes after more than the 50 ms an escape
# sequence gets to finish.
os.write(r.fd, b'\x1b[200~' + PASTED[:14].replace('\n', '\r').encode())
r.until('\x00', timeout=0.5)
shown = r.send(PASTED[14:].replace('\n', '\r') + '\x1b[201~', wait='/cost⏎▏')
check('the paste stays on the row, unsent, its line breaks shown as ⏎',
      'fix this:⏎    indented⏎⏎/cost⏎▏' in plain(shown) and 'queued' not in plain(shown), plain(shown)[-400:])
r.send('\r', wait='queued: fix this: indented /cost')
# Reading the terminal while the turn ends: low blocks on a full one.
r.until('done first')
sent = e.entries()[1]['body']['messages'] if len(e.entries()) > 1 else []
check('Enter sends it whole, as one steer after the run\'s result',
      [m['role'] for m in sent[-2:]] == ['tool', 'user'] and sent[-1]['content'] == PASTED.strip(), sent[-2:])
check('it is logged as a steer', (PASTED.strip(), True) in users(), users())

at = r.out.rindex('queued: fix this')
r.until('\x1b[r', start=at)
back = r.out.find('\x1b[r', at)
r.until('› ', start=back)
time.sleep(1)
r.until('\x00', timeout=0.3)
check('the turn ends with the terminal no longer marking pastes', '\x1b[?2004l' in r.out[at:back], r.out[at:back][-300:])
check('the /cost line went with the steer: nothing runs after the turn',
      len(e.entries()) == 2 and 'requests · in' not in plain(r.out[back:]), (len(e.entries()), plain(r.out[back:])))

os.write(r.fd, b'\x04')
r.until('\x00', timeout=5)
code = os.waitpid(r.pid, 0)[1]
check('the REPL ends normally', code == 0, code)
done()
