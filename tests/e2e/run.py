"""Run the end-to-end tests: python3 tests/e2e/run.py [--slow] [name ...]

Each test_*.py drives the low binary ($LOW_BIN, else target/debug/low; build it first) against mock.py in a throwaway
HOME and repo, and prints a PASS or FAIL line per check. Names pick tests (e.g. `stream budget`); --slow adds the
test_slow_* ones, which take minutes. Exits 1 if any check failed. low.toml's check can't run these: a sandboxed
check would start sandboxed lows inside it, and macOS's sandbox-exec does not nest."""
import glob, os, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
args = sys.argv[1:]
slow = '--slow' in args
names = [a for a in args if a != '--slow']
tests = {os.path.basename(t)[5:-3]: t for t in sorted(glob.glob(os.path.join(HERE, 'test_*.py')))}
unknown = [n for n in names if n not in tests]
if unknown:
    sys.exit(f'no such test: {", ".join(unknown)} (there are {", ".join(tests)})')
tests = [t for n, t in tests.items() if n in names or not names and (slow or not n.startswith('slow_'))]
failed = []
began = time.time()
for test in tests:
    name = os.path.basename(test)[5:-3]
    print(f'== {name}', flush=True)
    t = time.time()
    try:
        code = subprocess.run([sys.executable, test], cwd=HERE, timeout=900).returncode
    except subprocess.TimeoutExpired:
        print('FAIL timed out after 900 s')
        code = 1
    print(f'   {time.time() - t:.0f} s', flush=True)
    if code:
        failed.append(name)
print(f'{len(tests) - len(failed)}/{len(tests)} tests passed in {time.time() - began:.0f} s'
      + (f'; failed: {", ".join(failed)}' if failed else ''))
sys.exit(1 if failed else 0)
