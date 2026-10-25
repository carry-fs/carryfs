#!/usr/bin/env python3
"""Repeat the archived CarryFS workloads on Linux with stable Rust."""
import argparse
import csv
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import random
import statistics
import subprocess
import time

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
WORK = REPO / 'build/bbtree-fs-evaluation'
BUILD = WORK / 'stable'
FIELDS = ['block', 'pair', 'suite', 'case', 'size', 'mode', 'variant',
          'ns', 'wall_ns', 'iterations', 'checksum']
CORE = [('create', 20000), ('small-write', 4000), ('churn', 4000),
        ('large-write', 512), ('partial-overwrite', 256)]
FILES = [('create', 300), ('small-write', 100), ('churn', 100), ('large-write', 32)]
MAPS = [(case, n, 'ordered') for n in [1, 32, 256, 1024, 4096]
        for case in ['data_index', 'assignments', 'updates', 'pending']]
MAPS += [(case, n, 'shuffled') for n in [256, 1024]
         for case in ['data_index', 'assignments', 'updates', 'pending']]


def build():
    spec = importlib.util.spec_from_file_location('restore', HERE.parent / 'reproduce.py')
    restore = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(restore)
    restore.prepare()
    # Only the disposable source copy changes. Workload functions stay identical.
    source = (HERE.parent / 'runs/size-sweep/src/bin/maps.rs').read_text()
    source = source.replace('    fn pthread_set_qos_class_self_np(class: u32, priority: i32) -> i32;\n', '')
    source = source.replace('macOS clock 12', 'Linux clock 2').replace('clock_gettime(12,', 'clock_gettime(2,')
    source = source.replace('    // SAFETY: request user-initiated QoS for this thread using SDK enum 0x19.\n'
                            '    let status = unsafe { pthread_set_qos_class_self_np(0x19, 0) };\n'
                            '    assert_eq!(status, 0, "thread QoS request failed with errno {status}");\n', '')
    (WORK / 'size-sweep/src/bin/maps.rs').write_text(source)
    tool = lambda name: subprocess.check_output(['rustup', 'which', '--toolchain', 'stable', name], text=True).strip()
    cargo, rustc = tool('cargo'), tool('rustc')
    env = dict(os.environ, CARGO_HOME=str(WORK / 'cargo-home'), RUSTC=rustc)
    for suite, manifest in [('core', WORK / 'harness/Cargo.toml'), ('maps', WORK / 'size-sweep/Cargo.toml')]:
        for variant in ['std', 'bb']:
            subprocess.run([cargo, 'build', '--offline', '--locked', '--release',
                            '--manifest-path', str(manifest), '--target-dir', str(BUILD / f'{suite}-target-{variant}'),
                            '--no-default-features', '--features', variant + 'map', '--bins'], env=env, check=True)
    metadata = dict(rustc=subprocess.check_output([rustc,'--version','--verbose'],text=True),
                    cargo=subprocess.check_output([cargo,'--version'],text=True), platform=platform.platform(),
                    cpu=Path('/proc/cpuinfo').read_text().split('model name\t: ')[1].splitlines()[0],
                    rustflags=os.getenv('RUSTFLAGS'), carryfs_commit='df656f3d053167f4788d53e33a3ce7a8fdb0b03e',
                    bbtree_commit='8adae97a0ed97e5c98fa2790312eb0aec9719846')
    (BUILD/'metadata.json').write_text(json.dumps(metadata,indent=2)+'\n')


def statistics_for(pairs):
    std, bb = ([p[v] for p in pairs] for v in ['std', 'bb'])
    ratios = [p['std'] / p['bb'] for p in pairs]
    rng = random.Random(84013)
    bootstrap = []
    for _ in range(5000):
        sample = rng.choices(pairs, k=len(pairs))
        bootstrap.append(sum(p['std'] for p in sample) / sum(p['bb'] for p in sample))
    bootstrap.sort()
    return dict(pairs=len(pairs), std_mean_ns=statistics.mean(std), bb_mean_ns=statistics.mean(bb),
                std_median_ns=statistics.median(std), bb_median_ns=statistics.median(bb),
                speedup=statistics.mean(std)/statistics.mean(bb), median_paired=statistics.median(ratios),
                ci_low=bootstrap[125], ci_high=bootstrap[4875],
                std_cv=statistics.stdev(std)/statistics.mean(std), bb_cv=statistics.stdev(bb)/statistics.mean(bb),
                min_ratio=min(ratios), max_ratio=max(ratios),
                block_ratios={str(b): statistics.mean(p['std'] for p in pairs if p['block']==b) /
                              statistics.mean(p['bb'] for p in pairs if p['block']==b)
                              for b in sorted({p['block'] for p in pairs})})


def summarize(output):
    rows = list(csv.DictReader((output / 'raw.tsv').open(), delimiter='\t'))
    metadata = json.loads((output/'metadata.json').read_text())
    expected_pairs = metadata['blocks']*metadata['pairs_per_block']
    assert len(rows)==(len(CORE)*2+len(FILES)+len(MAPS))*expected_pairs*2
    groups = {}
    for r in rows:
        key = (r['suite'], r['case'], int(r['size']), r['mode'])
        point = groups.setdefault(key, {})
        pair = point.setdefault((int(r['block']), int(r['pair'])), {})
        assert r['variant'] not in pair
        assert int(r['iterations']) > 0 and float(r['ns']) > 0
        pair[r['variant']] = r
    result = []
    for (suite, case, size, mode), samples in groups.items():
        assert set(samples)=={(b,p) for b in range(metadata['blocks']) for p in range(metadata['pairs_per_block'])}
        assert len({r['checksum'] for pair in samples.values() for r in pair.values()})==1
        pairs = []
        for (block, pair), values in samples.items():
            assert set(values) == {'std', 'bb'}
            assert values['std']['checksum'] == values['bb']['checksum']
            pairs.append(dict(block=block, **{v: float(r['ns']) for v,r in values.items()}))
        item = dict(suite=suite, case=case, size=size, mode=mode, **statistics_for(pairs))
        item['wall_speedup'] = statistics.mean(float(p['std']['wall_ns']) for p in samples.values()) / statistics.mean(float(p['bb']['wall_ns']) for p in samples.values())
        if suite == 'maps':
            for v in ['std', 'bb']:
                item[v+'_cpu_fraction'] = statistics.mean(float(p[v]['ns'])/float(p[v]['wall_ns']) for p in samples.values())
        result.append(item)
    (output / 'summary.json').write_text(json.dumps(result, indent=2)+'\n')
    print(f'Validated {len(rows)} samples across {len(result)} configurations.', flush=True)
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--build', action='store_true')
    p.add_argument('--summarize', action='store_true')
    p.add_argument('--blocks', type=int, default=3)
    p.add_argument('--pairs', type=int, default=10)
    p.add_argument('--millis', type=int, default=300)
    p.add_argument('--cpu', type=int, default=2)
    p.add_argument('--output', type=Path, default=HERE / 'results')
    args = p.parse_args()
    assert platform.system() == 'Linux' and args.blocks > 0 and args.pairs >= 2 and args.millis >= 100
    if args.build:
        build()
        return
    if args.summarize:
        summarize(args.output)
        return
    args.output.mkdir(parents=True, exist_ok=True)
    if (args.output / 'raw.tsv').exists():
        raise SystemExit('Refusing to overwrite measurements. Choose a new --output directory.')
    os.sched_setaffinity(0, {args.cpu})
    metadata = json.loads((BUILD / 'metadata.json').read_text())
    metadata.update(blocks=args.blocks, pairs_per_block=args.pairs, minimum_map_wall_ms=args.millis,
                    warmup_ms=100, cpu_affinity=sorted(os.sched_getaffinity(0)),
                    started_utc=time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
                    map_metric='process CPU ns per complete lifecycle', core_metric='wall ns per workload',
                    ordering='three independently seeded case orderings; A/B order alternates each pair')
    metadata['binary_sha256'] = {f'{s}-{v}': hashlib.sha256((BUILD/f'{s}-target-{v}/release'/('core' if s=='core' else 'maps')).read_bytes()).hexdigest()
                                 for s in ['core','maps'] for v in ['std','bb']}
    metadata['inherited_allocator_settings'] = {k:v for k,v in os.environ.items() if k.startswith('MALLOC_') or k=='GLIBC_TUNABLES'}
    source = (WORK/'size-sweep/src/bin/maps.rs').read_text().split('#[repr(C)]')[0]
    original = (HERE.parent/'runs/size-sweep/src/bin/maps.rs').read_text().split('#[repr(C)]')[0]
    assert source==original
    metadata['map_functions_sha256'] = hashlib.sha256(source.encode()).hexdigest()
    (args.output / 'metadata.json').write_text(json.dumps(metadata, indent=2)+'\n')
    with (args.output / 'raw.tsv').open('w') as stream:
        writer = csv.DictWriter(stream, fieldnames=FIELDS, delimiter='\t')
        writer.writeheader()
        for block in range(args.blocks):
            for suite in ['core', 'file', 'maps']:
                if suite == 'core':
                    cases = [(c, 64 if c=='large-write' and not d else 32 if c=='partial-overwrite' and not d else n, str(d).lower())
                             for c,n in CORE for d in [True,False]]
                elif suite == 'file':
                    cases = [(c,n,'true') for c,n in FILES]
                else:
                    cases = MAPS.copy()
                random.Random(1327+block).shuffle(cases)
                for case, n, mode in cases:
                    for pair in range(-1 if suite!='maps' else 0, args.pairs):
                        checks = []
                        for variant in (['std','bb'] if (pair+block)%2==0 else ['bb','std']):
                            if suite == 'maps':
                                command = [str(BUILD/f'maps-target-{variant}/release/maps'),case,str(n),mode,str(args.millis)]
                            else:
                                command = [str(BUILD/f'core-target-{variant}/release/core'),case,str(n),mode]
                                if suite == 'file':
                                    image = BUILD / f'{variant}-scratch.cfs'
                                    command.append(str(image))
                            fields = subprocess.check_output(command, text=True, timeout=180).strip().split('\t')
                            if suite == 'maps':
                                c,size,pat,wall,iters,checksum,cpu = fields
                                assert (c,int(size),pat)==(case,n,mode)
                                ns, wall_ns = int(cpu)/int(iters), int(wall)/int(iters)
                                assert int(wall)>=args.millis*1_000_000 and 0<int(cpu)<=int(wall)*1.05
                            else:
                                c,m,size,wall,checksum = fields
                                assert (c,int(size),m)==(case,n,mode)
                                iters, ns, wall_ns = 1, int(wall), int(wall)
                                if suite == 'file':
                                    image.unlink()
                            checks.append(checksum)
                            if pair >= 0:
                                writer.writerow(dict(block=block,pair=pair,suite=suite,case=case,size=n,mode=mode,
                                                     variant=variant,ns=ns,wall_ns=wall_ns,iterations=iters,checksum=checksum))
                                stream.flush()
                        assert checks[0] == checks[1], (suite,case,n,mode)
                    print(f'block={block+1}/{args.blocks} {suite} {case} n={n} {mode} done', flush=True)
            (args.output / f'proc-stat-block-{block}.txt').write_text(Path('/proc/stat').read_text())
    metadata['finished_utc'] = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())
    (args.output / 'metadata.json').write_text(json.dumps(metadata, indent=2)+'\n')
    summarize(args.output)


if __name__ == '__main__':
    main()
