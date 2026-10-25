#!/usr/bin/env python3
"""Measure map scaling with alternating A/B pairs and a declared local stop rule."""
import argparse
import csv
import json
from pathlib import Path
import random
import resource
import statistics
import subprocess

ROOT = Path(__file__).resolve().parent
SIZES = [1, 4, 32, 256, 1024, 4096, 16384, 65536, 262144, 1048576, 2097152, 4194304, 6291456]
CASES = ['data_index', 'assignments', 'updates', 'pending']
PATTERNS = ['ordered', 'shuffled']

def stats(samples):
    std = [s['std'] for s in samples]
    bb = [s['bb'] for s in samples]
    mean_std, mean_bb = statistics.mean(std), statistics.mean(bb)
    rng = random.Random(12345)
    ratios = []
    for _ in range(6000):
        selected = [samples[rng.randrange(len(samples))] for _ in samples]
        ratios.append(sum(p['std'] for p in selected)/sum(p['bb'] for p in selected))
    ratios.sort()
    return dict(std_ns=mean_std, bb_ns=mean_bb, speedup=mean_std/mean_bb,
                median_paired=statistics.median(s['std']/s['bb'] for s in samples),
                ci_low=ratios[int(.025*len(ratios))], ci_high=ratios[int(.975*len(ratios))],
                std_cv=statistics.stdev(std)/mean_std, bb_cv=statistics.stdev(bb)/mean_bb)

def update_stop(state, ratio):
    if state['anchor'] is None or ratio >= state['anchor'] * 1.05:
        state['anchor'], state['stale'] = ratio, 0
    else:
        state['stale'] += 1
    state['steps'] += 1
    return state['steps'] >= state['minimum'] and state['stale'] >= 2

def self_check():
    s=stats([{'std':20.,'bb':10.} for _ in range(7)])
    assert s['speedup']==s['ci_low']==s['ci_high']==2.
    state=dict(anchor=None,stale=0,steps=0,minimum=5)
    assert not any(update_stop(state,1.) for _ in range(4))
    assert update_stop(state,1.)
    state=dict(anchor=None,stale=0,steps=0,minimum=5)
    for ratio in [1.,1.1,1.2,1.3]: assert not update_stop(state,ratio)
    assert not update_stop(state,1.35)
    assert update_stop(state,1.34)

def main():
    self_check()
    parser=argparse.ArgumentParser()
    parser.add_argument('--millis', type=int, default=1000)
    parser.add_argument('--pairs', type=int, default=7)
    parser.add_argument('--resume',action='store_true')
    args=parser.parse_args()
    assert args.millis>=100 and args.pairs>=3
    output=ROOT/'scaling-raw.tsv'
    fields=['case','entries','pattern','pair','variant','elapsed_ns','cpu_elapsed_ns','iterations','ns_per_cycle','wall_ns_per_cycle','checksum']
    rows=[]
    if args.resume:
        with output.open() as stream: rows=list(csv.DictReader(stream,delimiter='\t'))
    states={(c,p):dict(anchor=None,stale=0,steps=0,minimum=5 if c=='pending' else 6,stopped=False) for c in CASES for p in PATTERNS}
    summary=[]
    with output.open('a' if args.resume else 'w') as stream:
        writer=csv.DictWriter(stream,fieldnames=fields,delimiter='\t')
        if not args.resume:writer.writeheader()
        for n in SIZES:
            for case in CASES:
                for pattern in PATTERNS:
                    state=states[case,pattern]
                    if state['stopped']:continue
                    if case=='pending' and n>65536:
                        state['stopped']=True
                        state['reason']='memory cap at 65,536 entries, plateau not established'
                        continue
                    prior=[r for r in rows if r['case']==case and int(r['entries'])==n and r['pattern']==pattern]
                    samples=[]
                    if prior:
                        assert len(prior)==args.pairs*2,'Refuse to resume a partially completed point'
                        for pair in range(args.pairs):
                            values={}
                            for r in prior:
                                if int(r['pair'])==pair:
                                    values[r['variant']]=float(r['ns_per_cycle'])
                                    values[r['variant']+'_wall']=float(r['wall_ns_per_cycle'])
                            assert set(values)=={'std','bb','std_wall','bb_wall'}
                            samples.append(values)
                    else:
                        print(f'START {case} {pattern} n={n}',flush=True)
                        expected=None
                        for pair in range(args.pairs):
                            values={}
                            for variant in (['std','bb'] if pair%2==0 else ['bb','std']):
                                result=subprocess.check_output([str(ROOT/f'target-{variant}/release/maps'),case,str(n),pattern,str(args.millis)],text=True,timeout=180)
                                (ROOT/'peak-memory.json').write_text(json.dumps(dict(cumulative_child_peak_rss_bytes=resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss,case=case,entries=n,variant=variant),indent=2)+'\n')
                                c,size,p,ns,iters,checksum,cpu_ns=result.strip().split('\t')
                                assert (c,int(size),p)==(case,n,pattern)
                                if expected is None:expected=checksum
                                assert expected==checksum,'Variant checksum mismatch'
                                per_cycle=int(cpu_ns)/int(iters)
                                wall_per_cycle=int(ns)/int(iters)
                                row=dict(case=case,entries=n,pattern=pattern,pair=pair,variant=variant,elapsed_ns=ns,cpu_elapsed_ns=cpu_ns,iterations=iters,ns_per_cycle=per_cycle,wall_ns_per_cycle=wall_per_cycle,checksum=checksum)
                                rows.append(row)
                                writer.writerow(row)
                                stream.flush()
                                values[variant]=per_cycle
                                values[variant+'_wall']=wall_per_cycle
                            samples.append(values)
                    item=dict(case=case,entries=n,pattern=pattern,pairs=args.pairs,millis=args.millis,**stats(samples))
                    item['wall_speedup']=statistics.mean(s['std_wall'] for s in samples)/statistics.mean(s['bb_wall'] for s in samples)
                    item['std_cpu_fraction']=statistics.mean(s['std']/s['std_wall'] for s in samples)
                    item['bb_cpu_fraction']=statistics.mean(s['bb']/s['bb_wall'] for s in samples)
                    summary.append(item)
                    state['stopped']=update_stop(state,item['speedup'])
                    if state['stopped']:state['reason']='two size increases without a 5% gain over the last material best'
                    elif n==SIZES[-1]:
                        state['stopped']=True
                        state['reason']=f'size cap at {SIZES[-1]:,} entries, plateau not established'
                    print(f"DONE {case:11s} {pattern:8s} n={n:7d} CPUavg={item['speedup']:.3f}x wall={item['wall_speedup']:.3f}x 95%=[{item['ci_low']:.3f},{item['ci_high']:.3f}] CV={item['std_cv']:.1%}/{item['bb_cv']:.1%}"+(' STOP' if state['stopped'] else ''),flush=True)
                    (ROOT/'scaling-summary.json').write_text(json.dumps(summary,indent=2)+'\n')
                    (ROOT/'stop-state.json').write_text(json.dumps({f'{c}/{p}':s for (c,p),s in states.items()},indent=2)+'\n')
    (ROOT/'stop-state.json').write_text(json.dumps({f'{c}/{p}':s for (c,p),s in states.items()},indent=2)+'\n')
    assert all(s['stopped'] for s in states.values())
    metadata_path=ROOT/'metadata.json'
    if metadata_path.exists():
        metadata=json.loads(metadata_path.read_text())
        metadata.update(points=len(summary),measured_process_runs=len(rows),pairs_per_point=args.pairs,minimum_wall_ms=args.millis,all_eight_local_plateaus_found=all(s.get('reason','').startswith('two size increases') for s in states.values()))
        if (ROOT/'peak-memory.json').exists():
            metadata['actual_peak_rss_bytes']=json.loads((ROOT/'peak-memory.json').read_text())['cumulative_child_peak_rss_bytes']
        metadata_path.write_text(json.dumps(metadata,indent=2)+'\n')
    print('FINISHED',flush=True)

if __name__=='__main__':main()
