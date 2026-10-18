"""A model that runs the same code to the same result 5 times in a row is stopped (exit 5); run numbers in a traceback
don't make results differ; the same code with changing output, or fewer repeats, goes on."""
from harness import Env, check, done

LIMIT = 'the model ran the same code to the same result 5 times in a row'
cases = [('the same failing run', [{'code': 'print(open("nope.txt").read())'}] * 12, True),
         ('the same traceback, other run numbers', [{'code': 'def f():\n    raise ValueError("bad")\nf()'}] * 12, True),
         ('the same run, changing output', [{'code': 'import time\nprint(time.time_ns())'}] * 8, False),
         ('the same run 4 times', [{'code': 'print(1)'}] * 4, False)]
for name, script, stops in cases:
    e = Env(script + [{'content': 'done'}])
    r = e.low('--json', 'explain it')
    if stops:
        check(f'{name}: stopped after 5', r['code'] == 5 and len(r['requests']) == 5 and LIMIT in str(r['result'].get('limit')),
              f'exit {r["code"]}, {len(r["requests"])} requests, {r["result"].get("limit")}')
    else:
        check(f'{name}: goes on', r['code'] == 0 and len(r['requests']) == len(script) + 1, f'exit {r["code"]}, {len(r["requests"])} requests')
done()
