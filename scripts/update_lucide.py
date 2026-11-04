"""Vendor pinned Lucide artwork and bake vector paths; tooling only, not used by builds.

Run with svgpathtools available (for example through target/icon-tools).
"""
import concurrent.futures
import datetime
import math
from pathlib import Path
import sys
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'target/icon-tools'))
from svgpathtools import svg2paths2, Line

REV = 'f12b0de177fbc2a6795e99be065887e72b237123'  # Lucide 0.468.0
BASE = f'https://raw.githubusercontent.com/lucide-icons/lucide/{REV}'
FEATHER_NOTICE = '''

Feather-derived icons: The MIT License (MIT)
Source: https://lucide.dev/license
Copyright (c) 2013-present Cole Bemis

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
'''
MAPPING = {
    'Selection':'mouse-pointer-2','TrackSelectFwd':'arrow-right-to-line','TrackSelectBack':'arrow-left-to-line',
    'Ripple':'arrow-right-left','Rolling':'columns-2','RateStretch':'move-horizontal','Remix':'audio-lines',
    'Razor':'scissors','Slip':'between-horizontal-start','Slide':'move-horizontal','Pen':'pen-tool','Rectangle':'rectangle-horizontal',
    'Ellipse':'circle','Hand':'hand','Zoom':'zoom-in','Type':'type','Play':'play','Pause':'pause','StepBack':'step-back',
    'StepFwd':'step-forward','GoToIn':'skip-back','GoToOut':'skip-forward','PrevEdit':'arrow-left-to-line','NextEdit':'arrow-right-to-line',
    'MarkIn':'brackets','MarkOut':'brackets','Marker':'bookmark','Pin':'pin','Insert':'between-horizontal-start','Overwrite':'replace',
    'Lift':'arrow-up-from-line','Extract':'scissors-line-dashed','Camera':'camera','Loop':'repeat','Wrench':'wrench','Plus':'plus',
    'Eye':'eye','EyeOff':'eye-off','Speaker':'volume-2','Mute':'volume-x','Lock':'lock-keyhole','Unlock':'lock-keyhole-open',
    'SyncLock':'lock','Mic':'mic','Folder':'folder','Film':'film','Sequence':'clapperboard','Audio':'music','Image':'image',
    'Search':'search','ListView':'list','IconView':'layout-grid','Freeform':'layout-dashboard','NewItem':'file-plus-2',
    'Trash':'trash-2','Home':'house','Workspaces':'panels-top-left','Hamburger':'menu','ChevronDown':'chevron-down',
    'ChevronRight':'chevron-right','Magnet':'magnet','Link':'link','ExternalLink':'external-link','Keyframe':'diamond',
    'Stopwatch':'timer','Fx':'wand-sparkles','Reset':'rotate-ccw','Close':'x','Fullscreen':'maximize','Export':'upload',
    'Import':'download','Gear':'settings','Info':'info','Captions':'captions','Adjust':'sliders-horizontal','Nest':'layers',
    'Undo':'undo-2','Redo':'redo-2','Bell':'bell','Chat':'message-circle','Globe':'globe','Code':'code-xml','Sparkle':'sparkles',
    'Grid':'grid-2x2','Square':'square','Proxy':'picture-in-picture-2','Offline':'unlink','TrackMaskBack':'rewind',
    'TrackMaskBackFrame':'step-back','TrackMaskFwdFrame':'step-forward','TrackMaskFwd':'fast-forward','SortIcons':'list-filter',
    'Automate':'list-video','Star':'star','ChevronLeft':'chevron-left','ArrowUp':'arrow-up','Drive':'hard-drive','Network':'network',
    'Clock':'clock','Monitor':'monitor','Sun':'sun','Moon':'moon','Eyedropper':'pipette',
}

def fetch(name):
    with urllib.request.urlopen(f'{BASE}/icons/{name}.svg', timeout=30) as response:
        return name, response.read()

def coordinate(value):
    text=f'{value/1.5:.6g}'
    return text if '.' in text or 'e' in text else text+'.0'

def main():
    destination = ROOT / 'assets/icons/lucide'
    destination.mkdir(parents=True,exist_ok=True)
    # Download everything before touching application code; failures leave the current renderer intact.
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        files = dict(pool.map(fetch,sorted(set(MAPPING.values()))))
    with urllib.request.urlopen(f'{BASE}/LICENSE',timeout=30) as response:
        (destination/'LICENSE.txt').write_bytes(response.read() + FEATHER_NOTICE.encode('utf-8'))
    notice = ROOT / 'NOTICE'
    heading = 'Lucide icons (pinned 0.468.0)'
    current = notice.read_text(encoding='utf-8')
    if heading not in current:
        notice.write_text(current + '\n\n' + heading + '\n' + '-' * len(heading) + '\n' +
                          (destination/'LICENSE.txt').read_text(encoding='utf-8'), encoding='utf-8')
    rows = []
    paths_by_name = {}
    for name,data in sorted(files.items()):
        path = destination/f'{name}.svg'
        path.write_bytes(data)
        path.with_suffix('.svg.attribution').write_text(
            f'asset: {name}.svg\ntitle: Lucide {name} icon\nauthor: Lucide Contributors; Cole Bemis (Feather-derived icons)\n'
            f'source: {BASE}/icons/{name}.svg\nlicense: ISC AND MIT\nlicense-file: assets/icons/lucide/LICENSE.txt\n'
            'added: 2026-11-02 by FilmCraft contributors\nnotes: Unmodified upstream SVG, pinned Lucide 0.468.0.\n',encoding='utf-8')
        rows.append(f'| `assets/icons/lucide/{name}.svg` | Lucide Contributors; Cole Bemis | Lucide 0.468.0 (`{REV}`) | ISC AND MIT (`assets/icons/lucide/LICENSE.txt`) |')
        paths,_,_ = svg2paths2(str(path))
        lines = []
        for path_data in paths:
            for subpath in path_data.continuous_subpaths():
                points = []
                for segment in subpath:
                    count = 1 if isinstance(segment,Line) else max(4,min(64,math.ceil(segment.length()*2)))
                    if not points: points.append(segment.start)
                    points.extend(segment.point(i/count) for i in range(1,count+1))
                if len(points)>1:
                    lines.append('&['+','.join(f'({coordinate(p.real)},{coordinate(p.imag)})' for p in points)+']')
        if not lines: raise ValueError(f'No geometry for {name}')
        paths_by_name[name] = '&['+','.join(lines)+']'
    source = ['// Generated from pinned Lucide SVGs by scripts/update_lucide.py. See assets/icons/lucide/LICENSE.txt.','#[rustfmt::skip]',
              '#[allow(clippy::approx_constant, reason = "SVG coordinates, not mathematical constants")]',
              "pub(super) fn paths(icon: super::Icon) -> &'static [&'static [(f32,f32)]] {", '    match icon {']
    for name in sorted(paths_by_name):
        variants = ' | '.join('super::Icon::'+key for key,value in MAPPING.items() if value==name)
        source.append(f'        {variants} => {paths_by_name[name]},')
    source.extend(['    }','}'])
    generated = ROOT/'crates/ui-egui/src/lucide_paths.rs'
    generated.write_text('\n'.join(source)+'\n',encoding='utf-8')
    generated.with_suffix('.rs.attribution').write_text(
        'asset: lucide_paths.rs\ntitle: Lucide vector geometry\nauthor: Lucide Contributors; Cole Bemis\n'
        f'source: https://github.com/lucide-icons/lucide/tree/{REV}/icons\nlicense: ISC AND MIT\n'
        'license-file: assets/icons/lucide/LICENSE.txt\nadded: 2026-11-02 by FilmCraft contributors\n'
        'notes: SVG paths sampled to polylines on a 16-unit design grid by scripts/update_lucide.py.\n',encoding='utf-8')
    index = ROOT/'ATTRIBUTION.md'
    text = index.read_text(encoding='utf-8')
    heading = '\n## Lucide functional icons\n'
    prefix,found,tail=text.partition(heading)
    next_section=tail.find('\n## ')
    suffix=tail[next_section:] if found and next_section>=0 else ''
    text=prefix
    rows.append('| `crates/ui-egui/src/lucide_paths.rs` | Lucide Contributors; Cole Bemis | Generated from the pinned Lucide SVGs above | ISC AND MIT (`assets/icons/lucide/LICENSE.txt`) |')
    index.write_text(text+heading+'\n| Asset | Author | Source | Licence |\n|---|---|---|---|\n'+'\n'.join(rows)+'\n'+suffix,encoding='utf-8')
    print(f'Vendored {len(files)} SVGs for {len(MAPPING)} icon roles.')

if __name__ == '__main__': main()
