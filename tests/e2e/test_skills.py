"""No skill is in the system prompt. A task that asks for one by name, next to the word skill, gets its SKILL.md in a
message of its own, the repository's or $HOME's; a task that doesn't ask gets none. The worker can read the home
directory's skills though ~/.claude stays hidden: its skills folder and the folders its skills link to are let through,
while its other files and a skill linked into another secret path stay unreadable and are never sent."""
import os
from harness import Env, check, done, user_messages

READ = ("import os\nh = os.path.expanduser('~')\n"
        "for f in ('.claude/skills/vid/SKILL.md', '.claude/skills/jev/SKILL.md', '.agents/skills/jev/SKILL.md',\n"
        "          '.claude/.credentials.json', '.claude/skills/sec/SKILL.md', '.ssh/skillish/SKILL.md'):\n"
        "    try:\n        print(f, 'READ', open(os.path.join(h, f)).read().count('---'))\n"
        "    except OSError as e:\n        print(f, type(e).__name__)\n"
        "show(os.path.join(h, '.claude/skills/vid/SKILL.md'), 1, 2)\n")
e = Env([{'code': READ}, {'content': 'done'}, {'content': 'ok'}],
        files={'README.md': 'hi\n', '.agents/skills/ours/SKILL.md': '---\nname: ours\ndescription: Repo skill.\n---\nOURS\n'})
e.write_home('.claude/skills/vid/SKILL.md', '---\nname: video\ndescription: >\n  Make a video,\n  or edit one.\n---\nbody\n')
e.write_home('.agents/skills/jev/SKILL.md', '---\nname: jev\ndescription: "Run jobs."\n---\nJEV\n')
os.symlink('../../.agents/skills/jev', os.path.join(e.home, '.claude/skills/jev'))
e.write_home('.claude/.credentials.json', '{"fake": "not a real token"}\n')
e.write_home('.ssh/skillish/SKILL.md', '---\nname: sec\ndescription: Hidden.\n---\nSECRET\n')
os.symlink(os.path.join(e.home, '.ssh/skillish'), os.path.join(e.home, '.claude/skills/sec'))
h = e.home

r = e.low('--json', 'look at the skills, with jev in mind')
system = r['requests'][0]['messages'][0]['content'] if r['requests'] else ''
check('no skill is in the system prompt', '# Skills' not in system and 'Repo skill.' not in system
      and 'Run jobs.' not in system and 'video' not in system, system[-600:])
sent = user_messages(r['requests'][0]) if r['requests'] else []
check('a task that names no skill next to the word gets none', sent == ['look at the skills, with jev in mind'] or
      (len(sent) == 1 and sent[0].startswith('look at the skills, with jev in mind')), sent)
out = (e.tools() or [''])[-1]
check('the home directory\'s skills read, linked ones too',
      all(f'{f} READ 2' in out for f in ('.claude/skills/vid/SKILL.md', '.claude/skills/jev/SKILL.md',
                                          '.agents/skills/jev/SKILL.md')), out)
check('the rest of ~/.claude stays hidden', '.claude/.credentials.json PermissionError' in out
      and 'not a real token' not in out, out)
check('a skill linked into ~/.ssh stays hidden', '.claude/skills/sec/SKILL.md PermissionError' in out
      and '.ssh/skillish/SKILL.md PermissionError' in out, out)
check('show() reads a home skill', 'home/.claude/skills/vid/SKILL.md: lines 1-2 of 7]' in out and '2: name: video' in out,
      out)

r = e.low('--json', 'Use the video and ours skills, and the sec skill.')
# low sends the task and what comes with it as one user message.
sent = '\n'.join(user_messages(r['requests'][0])) if r['requests'] else ''
check('each skill the task asks for comes with it, by name, the repository\'s too',
      sent.count('[The task asks for the ') == 2
      and '[The task asks for the ours skill. Its SKILL.md (.agents/skills/ours/SKILL.md) follows' in sent
      and '\n---\nOURS\n' in sent
      and f'[The task asks for the video skill. Its SKILL.md ({h}/.claude/skills/vid/SKILL.md) follows' in sent
      and sent.endswith('\n---\nbody'), sent)
check('a skill linked into a secret path is never sent', 'SECRET' not in sent and 'asks for the sec skill' not in sent, sent)
events = [x['text'] for x in e.log() if x.get('event') == 'skill']
check('each skill sent is an event', any(t.startswith('the video skill goes with the task') for t in events)
      and len(events) == 2, events)
done()
