#!/usr/bin/env python3
"""Run sequential, alternating A/B measurements of the isolated binaries."""
import argparse
import csv
import json
from pathlib import Path
import platform
import statistics
import subprocess
import time

ROOT = Path(__file__).resolve().parent

def invoke(variant, binary, *args):
    return subprocess.check_output([str(ROOT / f'target-{variant}/release' / binary), *map(str, args)], text=True)

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('suite', choices=['core', 'maps', 'file'])
    parser.add_argument('--repeats', type=int, default=9)
    parser.add_argument('--resume', action='store_true')
    args = parser.parse_args()
    output = ROOT / f'{args.suite}-raw.tsv'
    cases = [('create', 20000), ('small-write', 4000), ('churn', 4000), ('large-write', 512), ('partial-overwrite', 256)]
    if args.suite == 'file':
        cases = [('create', 300), ('small-write', 100), ('churn', 100), ('large-write', 32)]
    rows = []
    if args.resume:
        with output.open() as stream:
            for row in list(csv.reader(stream, delimiter='\t'))[1:]:
                row[0], row[5] = int(row[0]), float(row[5])
                rows.append(row)
    with output.open('a' if args.resume else 'w') as stream:
        writer = csv.writer(stream, delimiter='\t')
        if not args.resume: writer.writerow(['round', 'variant', 'case', 'size', 'mode', 'ns', 'checksum'])
        if args.suite == 'maps':
            checks = {}
            for round_id in range(-1, args.repeats):
                for variant in (['std','bb'] if round_id % 2 == 0 else ['bb','std']):
                    for line in invoke(variant, 'maps').splitlines():
                        case, n, pattern, ns, iterations, checksum = line.split('\t')
                        key = (case, n, pattern)
                        assert checks.setdefault(key, checksum) == checksum, (variant, key)
                        if round_id >= 0:
                            row = [round_id, variant, case, n, pattern, int(ns)/int(iterations), checksum]
                            rows.append(row)
                            writer.writerow(row)
                stream.flush()
                print(f'maps round {round_id} complete', flush=True)
        else:
            for case, count in cases:
                modes = [True, False] if args.suite == 'core' else [True]
                for deferred in modes:
                    current_count = count
                    if not deferred and case in ['large-write', 'partial-overwrite']:
                        current_count = 64 if case == 'large-write' else 32
                    complete = [r for r in rows if r[2] == case and str(r[3]) == str(current_count) and str(r[4]) == str(deferred).lower()]
                    if len(complete) == args.repeats * 2: continue
                    assert not complete, 'Refuse to resume a partially completed group'
                    for round_id in range(-1, args.repeats):
                        checks = []
                        for variant in (['std','bb'] if round_id % 2 == 0 else ['bb','std']):
                            path_args = []
                            if args.suite == 'file':
                                image = ROOT / f'{variant}-{case}-{round_id}.cfs'
                                path_args = [image]
                            line = invoke(variant, 'core', case, current_count, str(deferred).lower(), *path_args).strip()
                            outcase, mode, n, ns, checksum = line.split('\t')
                            checks.append(checksum)
                            if path_args: image.unlink()
                            if round_id >= 0:
                                row = [round_id, variant, outcase, n, mode, int(ns), checksum]
                                rows.append(row)
                                writer.writerow(row)
                        assert checks[0] == checks[1]
                        stream.flush()
                    print(f'{args.suite} {case} deferred={deferred} complete', flush=True)
    groups = {}
    for round_id, variant, case, size, mode, ns, _ in rows:
        groups.setdefault((case, str(size), str(mode)), {}).setdefault(variant, {})[round_id] = ns
    summary = []
    for (case, size, mode), variants in groups.items():
        ratios = [variants['std'][r]/variants['bb'][r] for r in variants['std']]
        std = list(variants['std'].values())
        bb = list(variants['bb'].values())
        item = dict(case=case, size=int(size), mode=mode, std_ns=statistics.median(std), bb_ns=statistics.median(bb), speedup=statistics.median(ratios), min_ratio=min(ratios), max_ratio=max(ratios), std_cv=statistics.stdev(std)/statistics.mean(std), bb_cv=statistics.stdev(bb)/statistics.mean(bb))
        summary.append(item)
        print(f"{case:18s} {size:5s} {mode:8s} {item['speedup']:.3f}x [{min(ratios):.3f}, {max(ratios):.3f}] std={item['std_ns']/1e6:.3f}ms bb={item['bb_ns']/1e6:.3f}ms CV={item['std_cv']:.1%}/{item['bb_cv']:.1%}")
    (ROOT / f'{args.suite}-summary.json').write_text(json.dumps(summary, indent=2)+'\n')

if __name__ == '__main__': main()
