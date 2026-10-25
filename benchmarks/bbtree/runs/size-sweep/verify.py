"""Verify stored samples, average ratios, stopping decisions, and source consistency."""
import csv
import hashlib
import json
import math
from pathlib import Path
import statistics
from collections import defaultdict
ROOT=Path(__file__).resolve().parent
rows=list(csv.DictReader((ROOT/'scaling-raw.tsv').open(),delimiter='\t'))
summary=json.loads((ROOT/'scaling-summary.json').read_text())
states=json.loads((ROOT/'stop-state.json').read_text())
metadata=json.loads((ROOT/'metadata.json').read_text())
groups=defaultdict(list)
for row in rows:
    assert int(row['elapsed_ns'])>=metadata['minimum_wall_ms']*1_000_000
    assert 0<int(row['cpu_elapsed_ns'])<=int(row['elapsed_ns'])*1.05
    assert int(row['iterations'])>0
    assert math.isclose(float(row['ns_per_cycle']),int(row['cpu_elapsed_ns'])/int(row['iterations']))
    assert math.isclose(float(row['wall_ns_per_cycle']),int(row['elapsed_ns'])/int(row['iterations']))
    groups[row['case'],int(row['entries']),row['pattern']].append(row)
assert len(summary)==len(groups)
assert len(rows)==len(summary)*metadata['pairs_per_point']*2
for point in summary:
    key=(point['case'],point['entries'],point['pattern'])
    samples=groups[key]
    assert len(samples)==metadata['pairs_per_point']*2,key
    assert len({r['checksum'] for r in samples})==1,key
    for pair in range(metadata['pairs_per_point']):
        assert {r['variant'] for r in samples if int(r['pair'])==pair}=={'std','bb'}
    std=statistics.mean(float(r['ns_per_cycle']) for r in samples if r['variant']=='std')
    bb=statistics.mean(float(r['ns_per_cycle']) for r in samples if r['variant']=='bb')
    assert math.isclose(point['std_ns'],std) and math.isclose(point['bb_ns'],bb)
    assert math.isclose(point['speedup'],std/bb)
for lane,state in states.items():
    assert state['steps']>=5 and state['stopped'],lane
    assert state['reason'].startswith('two size increases'),lane
    case,pattern=lane.split('/')
    points=[r for r in summary if r['case']==case and r['pattern']==pattern]
    assert len(points)==state['steps']
source=(ROOT/'src/bin/maps.rs').read_text().split('fn main()')[0]
before=(ROOT/'source-before-extension.rs.txt').read_text().split('fn main()')[0]
assert source==before
assert hashlib.sha256(source.encode()).hexdigest()==metadata['timed_functions_sha256']
assert metadata['actual_peak_rss_bytes']<=json.loads((ROOT/'extension.json').read_text())['memory_budget_bytes']
print(f"Verified {len(summary)} configurations, {len(rows)} samples, matching checksums, means, and all eight local plateaus.")
