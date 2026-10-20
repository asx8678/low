"""search() and fetch(): fetch() takes a URL a search returned or any https URL on a registry, CDN or docs host, and
refuses one the run made up for another host before it leaves the machine. An open host's URL low fetches itself and
gives back as served, so a JSON body with \\" in it parses, following redirects only on the open hosts; an HTML page, a
rate limit and a redirect elsewhere go to the tools API, whose escaping is undone, and a missing page or a binary file
fails without it. Without a search provider, only the open hosts' text is fetched, and the prompt says so."""
from harness import Env, check, done

CODE = r'''
import json
found = search("three.js latest release", count=99)
print("found", [r["url"] for r in found])
print("page", fetch(found[0]["url"]).strip())
latest = json.loads(fetch("https://registry.npmjs.org/three/latest"))
print("version", latest["version"], "|", latest["description"])
print("redirected", fetch("https://unpkg.com/three"))
print("html", fetch("https://docs.python.org/3/library/json.html").splitlines()[0])
print("limited", fetch("https://api.github.com/limited"))
for url in ("https://example.org/made-up", "http://registry.npmjs.org/three/latest",
            "https://api.github.com/repos/mrdoob/three.js/tarball", "https://registry.npmjs.org/three/-/three-0.186.1.tgz",
            "https://registry.npmjs.org/nothing-here"):
    try:
        fetch(url)
        print("fetched", url)
    except RuntimeError as e:
        print("refused", url, "|", e)
'''
e = Env([{'code': CODE}, {'content': 'done'}], search=True)
r = e.low('--json', 'which three.js is latest?')
out = (e.tools() or [''])[-1]
line = lambda start: next((l for l in out.splitlines() if l.startswith(start)), '')
asked = [x for x in e.entries() if 'tool' in x]
tools = [a['body'].get('url') for a in asked if a['tool'] == 'fetch']
direct = [a['body']['url'] for a in asked if a['tool'] == 'open']
system = next((b['messages'][0]['content'] for b in r['requests'] if 'messages' in b), '')
check('the turn completes', r['code'] == 0 and r['result'].get('answer') == 'done', r['result'])
check('the prompt names fetch()\'s open hosts and how to read a registry',
      'any URL on registry.npmjs.org' in system and "Package managers can't reach their registries" in system, system)
check('search() asks for at most 50 results', [a['body'].get('count') for a in asked][:1] == [50], asked)
check('a search result fetches through the tools API, unescaped',
      'page # r186\n\nthree.js r186 (0.186.1), see "notes".' in out, out)
check('a registry URL the run built is fetched as served, and its \\" survives so it parses',
      'version 0.186.1 | a "3D" library' in out, out)
check('a redirect on the open hosts is followed', line('redirected') == 'redirected export const REVISION = "186";', out)
check('an HTML page goes to the tools API, as markdown', line('html') == 'html # json', out)
check('a rate limit goes to the tools API', line('limited') == 'limited {"from": "the tools API"}', out)
check('a URL made up for another host is refused, naming the open hosts',
      'registry.npmjs.org' in line('refused https://example.org/'), out)
check('plain http is refused on an open host too', line('refused http://registry.npmjs.org/three/latest') != '', out)
check('a redirect off the open hosts goes to the tools API, which has no such page',
      '404' in line('refused https://api.github.com/repos/mrdoob/three.js/tarball'), out)
check('a binary file is refused', 'a binary file (application/octet-stream)' in line('refused https://registry.npmjs.org/three/-/'),
      out)
check('a missing page is a 404, without asking the tools API', '404' in line('refused https://registry.npmjs.org/nothing'), out)
check('the tools API gets the search result and what the open hosts sent on, and nothing refused',
      tools == ['https://example.org/three/releases#1', 'https://docs.python.org/3/library/json.html',
                'https://api.github.com/limited', 'https://api.github.com/repos/mrdoob/three.js/tarball'], tools)
check('low itself asks only the open hosts, following the redirect',
      direct == ['https://registry.npmjs.org/three/latest', 'https://unpkg.com/three',
                 'https://unpkg.com/three@0.186.1/build/three.module.js', 'https://docs.python.org/3/library/json.html',
                 'https://api.github.com/limited', 'https://api.github.com/repos/mrdoob/three.js/tarball',
                 'https://registry.npmjs.org/three/-/three-0.186.1.tgz', 'https://registry.npmjs.org/nothing-here'],
      direct)

OFF = r'''
import json
print("version", json.loads(fetch("https://registry.npmjs.org/three/latest"))["version"])
for f, arg in ((fetch, "https://docs.python.org/3/library/json.html"), (fetch, "https://example.org/three/releases"),
               (search, "three.js")):
    try:
        f(arg)
    except RuntimeError as e:
        print("failed", arg, "|", e)
'''
off = Env([{'code': OFF}, {'content': 'done'}])
r = off.low('--json', 'which three.js is latest? https://example.org/three/releases says')
out = (off.tools() or [''])[-1]
line = lambda start: next((l for l in out.splitlines() if l.startswith(start)), '')
system = next((b['messages'][0]['content'] for b in r['requests'] if 'messages' in b), '')
check('without search set up the prompt has fetch() for the open hosts and no search()',
      'search(query' not in system and 'fetch(url) -> str' in system and 'not HTML pages; any URL on registry.npmjs.org' in system
      and "fetch() reads them" in system, system[:3000])
check('and an open host\'s JSON is fetched', line('version') == 'version 0.186.1', out)
check('an HTML page says it needs the tools API', 'it is an HTML page; low reads that only through DeepSeek' in
      line('failed https://docs.python.org'), out)
check('another host\'s page says only the open hosts are read', 'fetch() reads only URLs on the open hosts' in
      line('failed https://example.org'), out)
check('search() is not set up', 'web search is not set up' in line('failed three.js'), out)
check('nothing went to the tools API', not any(x.get('tool') in ('fetch', 'search') for x in off.entries()), off.entries())
done()
