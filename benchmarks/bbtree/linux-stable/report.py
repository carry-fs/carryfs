#!/usr/bin/env python3
"""Check retained measurements and render their means, spread, and controls."""
from collections import defaultdict
import json
import statistics

from run import HERE, summarize, statistics_for

rows = summarize(HERE/'results')
metadata = json.loads((HERE/'results/metadata.json').read_text())
assert '1.99.1' in metadata['rustc'] and metadata['blocks']==3 and metadata['pairs_per_block']==10
text = '# Linux stable measurements\n\n'
text += 'Ratios divide std time by bbtree time; above 1 favors bbtree. Each configuration has 30 alternating A/B pairs in three separately ordered blocks. Core and file use wall time per workload; maps use process CPU time per lifecycle. No samples are discarded.\n\n'
for suite in ['core','file','maps']:
    text += f'## {suite}\n\n| Workload | Entries/count | Mode/order | std mean µs | bb mean µs | Mean ratio | Median paired | 95% interval | std CV | bb CV | Block mean ratios |\n|---|---:|---|---:|---:|---:|---:|---|---:|---:|---|\n'
    for r in sorted((r for r in rows if r['suite']==suite),key=lambda r:(r['case'],r['mode'],r['size'])):
        blocks=', '.join(f'{x:.3f}' for x in r['block_ratios'].values())
        text += f"| {r['case']} | {r['size']} | {r['mode']} | {r['std_mean_ns']/1000:.3f} | {r['bb_mean_ns']/1000:.3f} | {r['speedup']:.3f}× | {r['median_paired']:.3f}× | {r['ci_low']:.3f}–{r['ci_high']:.3f} | {r['std_cv']:.1%} | {r['bb_cv']:.1%} | {blocks} |\n"
    text+='\n'
text += 'Intervals use 5,000 paired bootstrap resamples with a fixed seed. They assume independent pairs and describe sampling variation within this experiment, not all VM states or allocator histories. The three block means expose time drift. CV is sample standard deviation divided by the mean. CPU timing excludes descheduling, but host contention can still affect CPU work.\n\n'

probes=json.loads((HERE/'diagnostics/probes.json').read_text())
for path in sorted((HERE/'diagnostics').glob('extra-allocator-*.json')):
    probes+=json.loads(path.read_text())
groups=defaultdict(lambda:defaultdict(dict))
allocs=[]
phases=defaultdict(list)
for r in probes:
    kind=r['kind']
    if kind in ['isolated','compiler-control']:
        f=r['output'].split('\t')
        assert len(f)==7
        c,n,p,wall,iterations,checksum,cpu=f
        assert 0<int(cpu)<=int(wall)*1.05 and int(iterations)>0
        key=(kind,r['policy'],c,int(n),p)
        groups[key][r['pair']][r['variant']]=(int(cpu)/int(iterations),checksum)
    elif kind=='multi-case':
        for line in r['output'].splitlines():
            c,n,p,wall,iterations,checksum=line.split('\t')
            groups[kind,r['policy'],c,int(n),p][r['pair']][r['variant']]=(int(wall)/int(iterations),checksum)
    elif kind=='allocations':
        c,n,allocations,requested_bytes,zeroed,aligned,checksum=r['output'].split('\t')
        allocs.append(dict(variant=r['variant'],case=c,size=int(n),allocations=int(allocations),
                          bytes=int(requested_bytes),zeroed=int(zeroed),aligned=int(aligned),checksum=checksum))
    elif kind=='phases':
        n,iters,build,overlay,group,drop,checksum=r['output'].split('\t')
        phases[r['policy'],int(n),r['variant']].append(dict(pair=r['pair'],checksum=checksum,
            build=int(build)/int(iters),overlay=int(overlay)/int(iters),group=int(group)/int(iters),drop=int(drop)/int(iters)))
    elif kind=='steady-lookup':
        n,batch,wall,operations,checksum=r['output'].split('\t')
        groups[kind,batch,'lookup',int(n),'50% hits'][r['pair']][r['variant']]=(int(wall)/int(operations),checksum)
controls=[]
for (kind,policy,c,n,order),pairs in groups.items():
    values=[]
    for pair,p in pairs.items():
        assert set(p)=={'std','bb'} and p['std'][1]==p['bb'][1]
        values.append(dict(block=0,std=p['std'][0],bb=p['bb'][0]))
    item=dict(kind=kind,policy=policy,case=c,size=n,order=order,**statistics_for(values))
    controls.append(item)
(HERE/'diagnostics/summary.json').write_text(json.dumps(controls,indent=2)+'\n')
text+='## Controlled probes\n\nPolicy is allocator setting for isolated and multi-case runs, compiler for compiler controls, and whether bbtree batches 64 lookups for lookup probes. The std batched-loop control still makes scalar gets. Lookup setup and destruction are outside the timer; map lifecycle probes include both.\n\n| Probe | Policy | Workload | Entries | Order | Pairs | Mean ratio | 95% interval | std CV | bb CV |\n|---|---|---|---:|---|---:|---:|---|---:|---:|\n'
for r in controls:
    if r['kind']=='multi-case' and not (r['case']=='pending' and r['size']==1024):
        continue
    text+=f"| {r['kind']} | {r['policy']} | {r['case']} | {r['size']} | {r['order']} | {r['pairs']} | {r['speedup']:.3f}× | {r['ci_low']:.3f}–{r['ci_high']:.3f} | {r['std_cv']:.1%} | {r['bb_cv']:.1%} |\n"
text+='\n## Allocation counts\n\nOne complete lifecycle with counting enabled; input keys are allocated before counting. Requested bytes sum allocation and reallocation requests, so they are not peak live memory or RSS. Aligned means requested alignment at least 512 bytes. These instrumented binaries do not produce the primary timings.\n\n| Workload | Entries | Variant | Allocation/reallocation calls | Requested bytes | Zeroed calls | Aligned calls |\n|---|---:|---|---:|---:|---:|---:|\n'
for r in allocs:
    counterparts=[a for a in allocs if (a['case'],a['size'])==(r['case'],r['size'])]
    assert len(counterparts)==2 and len({a['checksum'] for a in counterparts})==1
    if r['size'] not in [1,1024]:
        continue
    text+=f"| {r['case']} | {r['size']} | {r['variant']} | {r['allocations']} | {r['bytes']} | {r['zeroed']} | {r['aligned']} |\n"
text+='\n## Buffered-write phases\n\nThese instrumented wall-time probes split insertion and buffer allocation, read overlays, map consumption and grouping, and final checksum plus buffer destruction. Extra timers and the counting allocator change overhead; use the uninstrumented primary samples for speedup claims.\n\n| Policy | Entries | Variant | Build µs | Overlay µs | Group µs | Checksum/drop µs |\n|---|---:|---|---:|---:|---:|---:|\n'
for (policy,n,v),points in phases.items():
    assert len(points)==5
    assert len({p['checksum'] for (pol,size,_),ps in phases.items() if (pol,size)==(policy,n) for p in ps})==1
    text+=f'| {policy} | {n} | {v} | '+ ' | '.join(f'{statistics.mean(p[k] for p in points)/1000:.3f}' for k in ['build','overlay','group','drop'])+' |\n'
text+='\n## User and system CPU time\n\nThese separate process measurements include setup, warmup, and a 500 ms measurement window at 1,024 ordered pending writes. They locate CPU work rather than estimate a speedup.\n\n| Policy | Variant | User seconds | System seconds | System fraction |\n|---|---|---:|---:|---:|\n'
cpu=json.loads((HERE/'diagnostics/cpu-time-split.json').read_text())
assert len(cpu)==4 and len({r['output'].split('\t')[5] for r in cpu})==1
for r in cpu:
    total=r['user_seconds']+r['system_seconds']
    assert total>0
    text+=f"| {r['policy']} | {r['variant']} | {r['user_seconds']:.4f} | {r['system_seconds']:.4f} | {r['system_seconds']/total:.1%} |\n"
(HERE/'RESULTS.md').write_text(text)
print(HERE/'RESULTS.md')
