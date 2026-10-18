"""With HOME pointing at a scratch folder, the account's real home directory stays read-only: run there, low says so
on stderr and in the system prompt. The scripted reply is text only, so no code runs in the real home."""
import os, pwd
from harness import Env, check, done

home = os.path.realpath(pwd.getpwuid(os.getuid()).pw_dir)
e = Env([{'content': 'nothing to do'}])
r = e.low('--json', 'look', cwd=home)
system = next((m['content'] for b in r['requests'] for m in b['messages'] if m['role'] == 'system'), '')
check('the scratch HOME is not the account home', os.path.realpath(e.home) != home)
check('low warns that the real home is read-only', 'this is your home directory or above it' in r['err'], r['err'])
check('the system prompt says so', "This folder is the user's home directory or above it" in system, system[-400:])
done()
