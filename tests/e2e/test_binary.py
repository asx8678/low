"""A binary file the model's code changed shows as `path (binary)` in the result, the stray-files nudge still names
it, and show() of a binary file prints its size instead of its bytes."""
from harness import Env, check, done, user_messages

e = Env([{'code': "sh(\"printf '\\\\000\\\\003\\\\004' > img.bin\")"}, {'content': 'done'}, {'content': 'kept it'}],
        files={'img.bin': '\x00\x01\x02', 'README.md': 'hi\n'})
r = e.low('--json', 'update the image')
check('binary change listed as (binary)', r['result'].get('changes') == ['img.bin (binary)'], r['result'].get('changes'))
nudge = [u for q in r['requests'] for u in user_messages(q) if 'without edit() or write()' in u]
check('stray nudge names the binary file', bool(nudge) and 'img.bin.' in nudge[-1], nudge)

e = Env([{'code': "show('blob.dat')\nshow('lines.txt', 3, 4)"}, {'content': 'looked'}],
        files={'blob.dat': b'\x7fELF\x00\x01' * 100, 'lines.txt': ''.join(f'line {i}\n' for i in range(1, 11))})
r = e.low('--json', 'look at them')
out = (e.tools() or [''])[-1]
check('show() of a binary file prints its size', '[blob.dat: binary, 600 bytes]' in out and 'ELF' not in out, out)
check('show() of a range still prints those lines', 'line 3' in out and 'line 4' in out and 'line 5' not in out, out)
done()
