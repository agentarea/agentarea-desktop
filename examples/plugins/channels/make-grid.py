"""Build the standalone grid.html from the plugin page: index.html with grid.js inlined.

    python3 make-grid.py [data-folder]      # default ~/AgentArea/channels

The page picks its file source itself: bridge inside the app, fetch when opened directly
(`cd ~/AgentArea/channels && python3 -m http.server 8765` → http://localhost:8765/grid.html).
"""
import os, sys

here = os.path.dirname(os.path.abspath(__file__))
out_dir = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser('~/AgentArea/channels')
page = open(os.path.join(here, 'index.html'), encoding='utf-8').read()
core = open(os.path.join(here, 'grid.js'), encoding='utf-8').read()
for tag in ('<script src="bridge.js"></script>\n', '<script src="grid.js"></script>\n'):
    assert tag in page, tag
page = page.replace('<script src="bridge.js"></script>\n', '')
page = page.replace('<script src="grid.js"></script>\n', '<script>\n' + core + '</script>\n')
tmp = os.path.join(out_dir, 'grid.html.tmp')
open(tmp, 'w', encoding='utf-8').write(page)
os.replace(tmp, os.path.join(out_dir, 'grid.html'))
print('wrote', os.path.join(out_dir, 'grid.html'))
