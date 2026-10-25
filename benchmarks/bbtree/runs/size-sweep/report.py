"""Render the completed scaling measurements without changing their data."""
from pathlib import Path
import json
ROOT=Path(__file__).resolve().parent
rows=json.loads((ROOT/'scaling-summary.json').read_text())
calibration=json.loads((ROOT/'calibration-summary.json').read_text())
states=json.loads((ROOT/'stop-state.json').read_text())
assert all(s['stopped'] for s in states.values()),'The sweep has not finished'
for name in ['data_index','assignments','updates','pending']:
    for pattern in ['ordered','shuffled']:
        points=[r for r in rows if r['case']==name and r['pattern']==pattern]
        assert len(points)>=(5 if name=='pending' else 6)
metadata=json.loads((ROOT/'metadata.json').read_text())
local_plateaus=sum(s.get('reason','').startswith('two size increases') for s in states.values())
def best(case):
    return max(r['speedup'] for r in rows if r['case']==case and r['pattern']=='ordered')
tail=sorted((r for r in rows if r['case']=='updates' and r['pattern']=='shuffled'),key=lambda r:r['entries'])
overview=f"The completed sweep covers {len(rows)} configurations and {sum(r['pairs']*2 for r in rows)} measured process runs. {local_plateaus} of {len(states)} map and key-order combinations met the declared local plateau rule. Ordered data indexes peaked at {best('data_index'):.3f}x, assignments at {best('assignments'):.3f}x, and pending writes at {best('pending'):.3f}x. Shuffled cleanup updates changed from {tail[-2]['speedup']:.3f}x at {tail[-2]['entries']:,} entries to {tail[-1]['speedup']:.3f}x at {tail[-1]['entries']:,}. Peak process RSS was {metadata['actual_peak_rss_bytes']/(1<<30):.2f} GiB."
if all(r['speedup']<1 for r in rows if r['entries']<=4):
    overview+=' Maps with one to four entries favored std in every model.'
text=f'''CarryFS map scaling with bbtree

{overview}

This experiment measures complete in-memory map lifecycles using CarryFS key and value shapes. It does not measure mounted filesystem throughput.

A separate calibration build traced the maps during five real Volume API workloads over a 256 MiB RAM block device. These occupancies describe those benchmark workloads, rather than a live mount or every possible user workload. Empty observations are retained in the calibration JSON. The percentiles below use nonempty maps only.

| Calibration workload | Map | Nonempty observations | Median entries | 90th percentile | Maximum |
|---|---|---:|---:|---:|---:|
'''
for r in calibration:
    if r['nonempty']:
        text+=f"| {r['case']} | {r['map']} | {r['nonempty']:,} | {r['p50_nonempty']:,} | {r['p90_nonempty']:,} | {r['max']:,} |\n"
text+='''
The size sweep starts at 1, 4, 32, 256, 1,024, and 4,096 entries. Active cases then grow fourfold through 1,048,576 entries. The remaining active case then grows to 2,097,152, 4,194,304, and 6,291,456 entries. Each core map gets at least six size points, and the pending-write map gets at least five through 1,024 entries. A case stops after two successive size increases fail to produce a cumulative 5% gain over its last material best speedup. This is a local plateau under the tested sizes and operation mix. A later cache or allocator transition could change the result.

Each point has seven sequential A/B pairs, with variant order alternating by pair. Every process warms for 100 ms, then measures at least one second of wall time. Timer checks are amortized over batches at small sizes. Raw results retain all samples, operation counts, checksums, CPU time, and wall time.

The primary ratio is average std process-CPU nanoseconds per lifecycle divided by average bbtree process-CPU nanoseconds per lifecycle. A ratio above 1 favors bbtree. Intervals are 95% paired bootstrap intervals over the seven repetitions, using 6,000 resamples and a fixed seed. They describe each tested point, rather than a global confidence guarantee for the adaptive stopping procedure. Median paired ratios, coefficients of variation, CPU utilization, and wall-time ratios are also in the summary JSON.

The host showed scheduling noise during initial diagnostics. The final harness records macOS process CPU time with clock_gettime and requests user-initiated QoS for its own benchmark thread. Both variants receive the same treatment. CPU time excludes time waiting to run but still depends on CPU frequency and core selection. The earlier timer and wall-time diagnostic probes are retained separately and excluded from this final sweep.

The source is the same pinned bbtree 0.1.5 commit 8adae97a0ed97e5c98fa2790312eb0aec9719846 used in the prior comparison. The host is Apple M1 with 16 GB RAM, macOS 15.6, Rust 1.99.0, release optimization. No benchmark, build, or calibration run executes concurrently with these samples.

The simulated operations are unchanged from the prior comparison. Data indexes use duplicate checks, inserts, range scans, removals, and destruction. Assignments use ordered collection, extension, existence checks, iteration, and lookups. Cleanup updates use 17-byte keys and optional 32-byte values, including allocation and replacement. Pending writes use 4 KiB Vec buffers and the actual adapter overlay and run-grouping functions, including buffer allocation and destruction. Fixed 8 KiB overlay windows are one modeled read shape.

Assignment IDs come from BTreeSet. Initial collection and later extension each consume sorted IDs. The ordered simulation models that structure, but uses consecutive halves rather than a recorded ID trace. The shuffled assignment case is stress testing. Data-index and cleanup insertion orders depend on allocation and transaction history, so both orders are retained.

For data indexes and final assignments, sizes above 4,096 exceed the current accepted-plan bound. The calibration's largest data index was only 256 entries. Pending writes above 1,024 entries exceed the normal 4 MiB threshold for the simulated 4 KiB writes. Values beyond those bounds test scaling, rather than an expected current CarryFS workload. The pending-write stress test has a 65,536-entry memory cap. The sweep was extended to 6,291,456 entries because shuffled cleanup updates still improved at the original million-entry limit. A separate memory probe at 1,048,576 entries observed peak process RSS of about 156 MiB for std and 255 MiB for bbtree. The four-million-entry run used about 1.0 GiB peak process RSS. The final six-million-entry cap was selected to keep expected peak process memory within the 1.5 GiB budget on this loaded host. Peak RSS includes the whole benchmark process and allocator caches. A size cap is reported separately if the local plateau was not established.

The following table gives the average CPU speedup at every measured size. A dash means that case had already stopped or reached its memory cap.

| Entries | Data ordered | Data shuffled | Assignments ordered | Assignments shuffled | Updates ordered | Updates shuffled | Pending ordered | Pending shuffled |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
'''
lookup={(r['case'],r['pattern'],r['entries']):r for r in rows}
for n in sorted({r['entries'] for r in rows}):
    vals=[]
    for name in ['data_index','assignments','updates','pending']:
        for pattern in ['ordered','shuffled']:
            r=lookup.get((name,pattern,n))
            vals.append(f"{r['speedup']:.3f}x" if r else '-')
    text+=f"| {n:,} | "+' | '.join(vals)+' |\n'
text+='''
This summary identifies the first tested size whose entire interval favors bbtree, the best observed average, and the largest tested size. Intermediate crossover sizes were not tested. Some curves drop below 1 again at larger sizes, so the first tested win applies to that point only. Selecting the largest observed ratio can overestimate the true peak.

| Map | Order | First tested win with interval above 1 | Best observed average | Largest tested size | Stop reason |
|---|---|---:|---|---:|---|
'''
for name in ['data_index','assignments','updates','pending']:
    for pattern in ['ordered','shuffled']:
        points=[r for r in rows if r['case']==name and r['pattern']==pattern]
        best=max(points,key=lambda r:r['speedup'])
        wins=[r['entries'] for r in points if r['ci_low']>1]
        reason=states[f'{name}/{pattern}']['reason']
        text+=f"| {name} | {pattern} | {min(wins) if wins else 'none'} | {best['speedup']:.3f}x at {best['entries']:,} | {max(r['entries'] for r in points):,} | {reason} |\n"
text+='''
Every measured point follows, including uncertainty and average latency. These are microbenchmarks of whole lifecycles, so nanoseconds per lifecycle increase with map size.

| Map | Order | Entries | std CPU, microseconds | bbtree CPU, microseconds | Average speedup | 95% interval | Wall-time speedup |
|---|---|---:|---:|---:|---:|---|---:|
'''
for r in rows:
    text+=f"| {r['case']} | {r['pattern']} | {r['entries']:,} | {r['std_ns']/1000:.3f} | {r['bb_ns']/1000:.3f} | {r['speedup']:.3f}x | {r['ci_low']:.3f} to {r['ci_high']:.3f}x | {r['wall_speedup']:.3f}x |\n"
text+='''
The original repository remains unchanged. All work is in this temporary experiment directory. The source snapshots and dependencies remain in its parent directory. Rebuild and rerun on this macOS host with:

```sh
python3 build.py
python3 sweep.py --millis 1000 --pairs 7
python3 report.py
python3 verify.py
```

`--resume` reuses complete recorded points and recalculates the same stopping decisions. It refuses partially completed points. The statistics and stopping-rule self-checks run before every sweep, and every benchmark iteration checks its result. Variant checksums must match for each point. Final verification also checks pair counts and durations.

[Calibration data](calibration-summary.json), [complete raw measurements](scaling-raw.tsv), [summary and confidence intervals](scaling-summary.json), [stop decisions](stop-state.json), [benchmark source](src/bin/maps.rs), and [runner](sweep.py) are retained for review.

These results do not establish a filesystem throughput improvement. Mounted FUSE, WinFsp, other platforms, and CPU or memory profiles were not measured in this sweep.
'''
(ROOT/'report.md').write_text(text)
print(ROOT/'report.md')
