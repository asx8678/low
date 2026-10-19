"""search() and fetch() through the provider's tools API: fetch() takes a URL a search returned or any https URL on a
registry, CDN or docs host, refuses one the run made up for another host before it leaves the machine, and gives the
page back without DeepSeek's escaping, so a JSON API's body parses. The prompt says all this only when search is set."""
from harness import Env, check, done

CODE = r'''
import json
found = search("three.js latest release", count=99)
print("found", [r["url"] for r in found])
print("page", fetch(found[0]["url"]).strip())
latest = json.loads(fetch("https://registry.npmjs.org/three/latest"))
print("version", latest["version"], "|", latest["description"])
for url in ("https://example.org/made-up", "http://registry.npmjs.org/three/latest"):
    try:
        fetch(url)
        print("fetched", url)
    except RuntimeError as e:
        print("refused", url, "|", e)
'''
e = Env([{'code': CODE}, {'content': 'done'}], config='search = "mock"')
r = e.low('--json', 'which three.js is latest?')
out = (e.tools() or [''])[-1]
asked = [x for x in e.entries() if 'tool' in x]
system = next((b['messages'][0]['content'] for b in r['requests'] if 'messages' in b), '')
check('the turn completes', r['code'] == 0 and r['result'].get('answer') == 'done', r['result'])
check('the prompt names fetch()\'s open hosts and how to read a registry',
      'any URL on registry.npmjs.org' in system and "Package managers can't reach their registries" in system, system)
check('search() asks for at most 50 results', [a['body'].get('count') for a in asked][:1] == [50], asked)
check('a search result fetches, unescaped', 'page # r186\n\nthree.js r186 (0.186.1), see "notes".' in out, out)
check('a registry URL the run built fetches, and its JSON parses', 'version 0.186.1 | a "3D" library' in out, out)
made_up = next((line for line in out.splitlines() if line.startswith('refused https://example.org/')), '')
check('a URL made up for another host is refused, naming the open hosts', 'registry.npmjs.org' in made_up, out)
check('plain http is refused on an open host too', 'refused http://registry.npmjs.org/three/latest' in out, out)
check('refused URLs never reach the tools API',
      [a['body'].get('url') for a in asked if a['tool'] == 'fetch'] ==
      ['https://example.org/three/releases#1', 'https://registry.npmjs.org/three/latest'], asked)

off = Env([{'content': 'done'}])
r = off.low('--json', 'hi')
system = next((b['messages'][0]['content'] for b in r['requests'] if 'messages' in b), '')
check('without search set up the prompt has no web helpers',
      bool(system) and 'fetch(url)' not in system and 'registries' not in system, system[:2000])
done()
