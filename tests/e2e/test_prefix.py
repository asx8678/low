"""The prompt cache golden check: in a session without compaction or a model or effort switch, every request's
messages start the next one's unchanged (each compared as sent, key order included), across turns and a --resume,
with the same tools and parameters; reasoning goes back whole, however long."""
import json
from harness import Env, check, done

LONG = 'Thinking it through. ' * 1000  # 21 KB, past the 16 KB the old build clipped to
script = [
    {'reasoning': 'Let me look at calc.py first. 思考 🤔', 'content': 'Looking.', 'code': 'show("calc.py")'},
    {'reasoning': LONG, 'code': 'accept("add(2, 3) == 5")\nedit("calc.py", "return a - b", "return a + b")\nimport calc\n'
     'assert calc.add(2, 3) == 5\nprint("ok")'},
    {'content': 'add adds now.'},
    {'reasoning': 'Read it again.', 'code': 'print(open("calc.py").read())'},
    {'content': 'add returns a + b.'},
    {'code': 'print(1)'},
    {'content': 'ok'},
]
e = Env(script, files={'calc.py': 'def add(a, b):\n    return a - b\n'})
codes = [e.low('--json', 'fix add')['code'], e.low('--json', '--resume', 'what does add return?')['code'],
         e.low('--json', '--resume', 'print 1')['code']]
check('three turns, two of them resumed, complete', codes == [0, 0, 0], codes)
raws = [json.loads(x['raw']) for x in e.entries()]  # json.loads keeps the key order as sent
wire = lambda m: json.dumps(m, ensure_ascii=False, separators=(',', ':'))
broken = []
for k, (a, b) in enumerate(zip(raws, raws[1:])):
    ma, mb = [wire(m) for m in a['messages']], [wire(m) for m in b['messages']]
    if mb[:len(ma)] != ma:
        at = next((i for i, (x, y) in enumerate(zip(ma, mb)) if x != y), len(mb))
        broken.append((k, k + 1, f'message {at} differs: {ma[at][:120] if at < len(ma) else ""} | {mb[at][:120] if at < len(mb) else "missing"}'))
    rest = lambda body: {key: v for key, v in body.items() if key != 'messages'}
    if wire(rest(a)) != wire(rest(b)):
        broken.append((k, k + 1, 'tools or parameters changed'))
check(f'each of {len(raws)} requests starts the next one', len(raws) == 7 and not broken, broken or len(raws))
echoed = [m.get('reasoning_content', '') for m in raws[2]['messages'] if m['role'] == 'assistant']
check('a 21 KB reasoning goes back whole', LONG in echoed, [len(x) for x in echoed])
done()
