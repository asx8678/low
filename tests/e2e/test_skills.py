"""Skills are listed in the system prompt, the repository's and $HOME's, and the worker can read the home directory's
though ~/.claude stays hidden: its skills folder and the folders its skills link to are let through, while its other
files and a skill linked into another secret path stay unreadable and unlisted."""
import os
from harness import Env, check, done

READ = ("import os\nh = os.path.expanduser('~')\n"
        "for f in ('.claude/skills/vid/SKILL.md', '.claude/skills/jev/SKILL.md', '.agents/skills/jev/SKILL.md',\n"
        "          '.claude/.credentials.json', '.claude/skills/sec/SKILL.md', '.ssh/skillish/SKILL.md'):\n"
        "    try:\n        print(f, 'READ', open(os.path.join(h, f)).read().count('---'))\n"
        "    except OSError as e:\n        print(f, type(e).__name__)\n"
        "show(os.path.join(h, '.claude/skills/vid/SKILL.md'), 1, 2)\n")
e = Env([{'code': READ}, {'content': 'done'}],
        files={'README.md': 'hi\n', '.agents/skills/ours/SKILL.md': '---\nname: ours\ndescription: Repo skill.\n---\n'})
e.write_home('.claude/skills/vid/SKILL.md', '---\nname: video\ndescription: >\n  Make a video,\n  or edit one.\n---\nbody\n')
e.write_home('.agents/skills/jev/SKILL.md', '---\nname: jev\ndescription: "Run jobs."\n---\n')
os.symlink('../../.agents/skills/jev', os.path.join(e.home, '.claude/skills/jev'))
e.write_home('.claude/.credentials.json', '{"fake": "not a real token"}\n')
e.write_home('.ssh/skillish/SKILL.md', '---\nname: sec\ndescription: Hidden.\n---\n')
os.symlink(os.path.join(e.home, '.ssh/skillish'), os.path.join(e.home, '.claude/skills/sec'))
r = e.low('--json', 'look at the skills')
system = r['requests'][0]['messages'][0]['content'] if r['requests'] else ''
h = e.home
check('the skills are listed, the repository\'s first, each once',
      f'# Skills' in system and '- ours (.agents/skills/ours/SKILL.md): Repo skill.\n' in system
      and f'- jev ({h}/.agents/skills/jev/SKILL.md): Run jobs.\n' in system
      and f'- video ({h}/.claude/skills/vid/SKILL.md): Make a video, or edit one.' in system
      and system.count('- jev (') == 1, system[system.find('# Skills'):][:800])
check('a skill linked into a secret path is not listed', '- sec (' not in system and 'Hidden.' not in system, '')
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
done()
