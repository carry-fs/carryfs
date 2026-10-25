#!/usr/bin/env python3
"""Controlled probes of compiler, allocator, process history, and map lifecycle."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import subprocess

from run import HERE, WORK, BUILD, CORE


def build():
    # Reuse the exact lifecycle functions; the diagnostic main is separate.
    source = (WORK / 'size-sweep/src/bin/maps.rs').read_text().split('#[repr(C)]')[0]
    target = BUILD / 'diagnostic.rs'
    target.write_text(source + (HERE / 'diagnostic.rs').read_text())
    rustc = subprocess.check_output(['rustup','which','--toolchain','stable','rustc'],text=True).strip()
    for variant in ['std','bb']:
        command = [rustc,'--edition=2024','-O','-g',str(target),'--cfg',f'feature="{variant}map"',
                   '-o',str(BUILD/f'diagnostic-{variant}')]
        if variant == 'bb':
            deps = BUILD/'maps-target-bb/release/deps'
            libraries = list(deps.glob('libbbtree-*.rlib'))
            assert len(libraries)==1
            command += ['-L',f'dependency={deps}','--extern',f'bbtree={libraries[0]}']
        subprocess.run(command,check=True)


def invoke(binary, args, env):
    return subprocess.check_output([str(binary),*map(str,args)],text=True,env=env,timeout=180).strip()


def record_cpu_times(output):
    rows=[]
    for policy in ['default','no-trim']:
        env=dict(os.environ)
        if policy=='no-trim':
            env['GLIBC_TUNABLES']=(env.get('GLIBC_TUNABLES','')+':glibc.malloc.trim_threshold=1073741824').lstrip(':')
        for variant in ['std','bb']:
            before=resource.getrusage(resource.RUSAGE_CHILDREN)
            line=invoke(BUILD/f'maps-target-{variant}/release/maps',['pending',1024,'ordered',500],env)
            after=resource.getrusage(resource.RUSAGE_CHILDREN)
            rows.append(dict(policy=policy,variant=variant,cpu_affinity=sorted(os.sched_getaffinity(0)),output=line,
                             user_seconds=after.ru_utime-before.ru_utime,system_seconds=after.ru_stime-before.ru_stime))
    (output/'cpu-time-split.json').write_text(json.dumps(rows,indent=2)+'\n')


def extra_allocator_probe(output, size):
    records=[]
    for policy in ['default','no-trim']:
        env=dict(os.environ)
        if policy=='no-trim':
            env['GLIBC_TUNABLES']=(env.get('GLIBC_TUNABLES','')+':glibc.malloc.trim_threshold=1073741824').lstrip(':')
        for pair in range(10):
            for variant in (['std','bb'] if pair%2==0 else ['bb','std']):
                params=['pending',size,'ordered',300]
                line=invoke(BUILD/f'maps-target-{variant}/release/maps',params,env)
                records.append(dict(kind='isolated',variant=variant,policy=policy,pair=pair,parameters=params,output=line))
        for variant in ['std','bb']:
            command=[str(BUILD/f'maps-target-{variant}/release/maps'),'pending',str(size),'ordered','1000']
            prefix=output/f'{policy}-{variant}-{size}'
            for kind,tool in [('strace',['strace','-c','-e','brk,mmap,munmap,madvise,mprotect','-o',str(prefix)+'.strace']),
                              ('perf-stat',['perf','stat','-x,','-e','task-clock,page-faults,minor-faults,major-faults,context-switches,cpu-migrations','-o',str(prefix)+'.perf'])]:
                line=subprocess.check_output([*tool,*command],env=env,text=True).strip()
                records.append(dict(kind=kind,variant=variant,policy=policy,pair=0,parameters=command[1:],output=line))
    (output/f'extra-allocator-{size}.json').write_text(json.dumps(records,indent=2)+'\n')


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--build',action='store_true')
    p.add_argument('--cpu',type=int,default=2)
    p.add_argument('--extra-size',type=int,help='Run only an additional buffered-write allocator control.')
    p.add_argument('--output',type=Path,default=HERE/'diagnostics')
    args=p.parse_args()
    if args.build:
        build()
        return
    os.sched_setaffinity(0,{args.cpu})
    args.output.mkdir(parents=True,exist_ok=True)
    if args.extra_size:
        assert 0<args.extra_size<=65536
        if (args.output/f'extra-allocator-{args.extra_size}.json').exists():
            raise SystemExit('Refusing to overwrite extra allocator samples.')
        extra_allocator_probe(args.output,args.extra_size)
        return
    if (args.output/'probes.json').exists():
        raise SystemExit('Refusing to overwrite diagnostic samples.')
    records=[]
    environments = {'default':dict(os.environ),
                    'no-trim':dict(os.environ,GLIBC_TUNABLES=(os.environ.get('GLIBC_TUNABLES','')+':glibc.malloc.trim_threshold=1073741824').lstrip(':'))}
    metadata=dict(cpu_affinity=sorted(os.sched_getaffinity(0)),stable=json.loads((BUILD/'metadata.json').read_text()),
                  allocator_settings={k:{n:v for n,v in e.items() if n.startswith('MALLOC_') or n=='GLIBC_TUNABLES'}
                                      for k,e in environments.items()})
    earlier=WORK/'linux-metadata.json'
    if earlier.exists():
        metadata['earlier_nightly']=json.loads(earlier.read_text())
    metadata['diagnostic_source_sha256']=hashlib.sha256((BUILD/'diagnostic.rs').read_bytes()).hexdigest()
    (args.output/'metadata.json').write_text(json.dumps(metadata,indent=2)+'\n')
    calibration=[]
    for case,n in CORE:
        result=subprocess.run([str(BUILD/'maps-target-std/release/calibrate'),case,str(n),'true'],
                              text=True,capture_output=True,check=True)
        (args.output/f'calibration-{case}.log').write_text(result.stdout+result.stderr)
        for kind,name in enumerate(['data_index','assignments','updates']):
            histogram={}
            for line in result.stderr.splitlines():
                tag,k,size,count=line.split('\t')
                assert tag=='map_size'
                if int(k)==kind:
                    histogram[int(size)]=int(count)
            nonempty=sum(count for size,count in histogram.items() if size)
            def percentile(fraction):
                cumulative=0
                for size,count in sorted(histogram.items()):
                    if size:
                        cumulative+=count
                        if cumulative>=nonempty*fraction:
                            return size
                return 0
            calibration.append(dict(case=case,map=name,histogram=histogram,nonempty=nonempty,
                                    p50_nonempty=percentile(.5),p90_nonempty=percentile(.9),max=max(histogram,default=0)))
    (args.output/'calibration.json').write_text(json.dumps(calibration,indent=2)+'\n')

    def save(kind,variant,policy,pair,parameters,line):
        records.append(dict(kind=kind,variant=variant,policy=policy,pair=pair,parameters=parameters,output=line))
        (args.output/'probes.json').write_text(json.dumps(records,indent=2)+'\n')

    for policy,env in environments.items():
        for pair in range(10):
            for variant in (['std','bb'] if pair%2==0 else ['bb','std']):
                binary=BUILD/f'maps-target-{variant}/release/maps'
                params=['pending',1024,'ordered',300]
                save('isolated',variant,policy,pair,params,invoke(binary,params,env))
        print(f'isolated {policy} done',flush=True)
        for variant in ['std','bb']:
            binary=BUILD/f'maps-target-{variant}/release/maps'
            params=['pending',1024,'ordered',1000]
            command=[str(binary),*map(str,params)]
            prefix=args.output/f'{policy}-{variant}'
            output=subprocess.check_output(['strace','-c','-e','brk,mmap,munmap,madvise,mprotect','-o',str(prefix)+'.strace',*command],env=env,text=True)
            save('strace',variant,policy,0,params,output.strip())
            output=subprocess.check_output(['perf','stat','-x,','-e','task-clock,page-faults,minor-faults,major-faults,context-switches,cpu-migrations','-o',str(prefix)+'.perf',*command],env=env,text=True)
            save('perf-stat',variant,policy,0,params,output.strip())
        for pair in range(5):
            for variant in (['std','bb'] if pair%2==0 else ['bb','std']):
                binary=BUILD/f'core-target-{variant}/release/maps'
                save('multi-case',variant,policy,pair,[],invoke(binary,[],env))
        print(f'multi-case {policy} done',flush=True)

    has_nightly=all((WORK/f'size-sweep/target-{v}/release/maps').is_file() for v in ['std','bb'])
    if not has_nightly:
        print('Earlier nightly binaries absent; only stable compiler controls will run.',flush=True)
    for case,n in [('data_index',1),('data_index',1024),('pending',1024)]:
        for pair in range(10):
            combinations=[('stable','std'),('stable','bb'),('nightly','std'),('nightly','bb')]
            if pair%2:
                combinations.reverse()
            for compiler,variant in combinations:
                if compiler=='nightly' and not has_nightly:
                    continue
                binary=(BUILD/f'maps-target-{variant}/release/maps' if compiler=='stable'
                        else WORK/f'size-sweep/target-{variant}/release/maps')
                params=[case,n,'ordered',300]
                save('compiler-control',variant,compiler,pair,params,
                     invoke(binary,params,environments['default']))
    for variant in ['std','bb']:
        for name in ['data_index','assignments','updates','pending']:
            for n in [1,32,256,1024,4096]:
                params=['alloc',name,n]
                save('allocations',variant,'default',0,params,
                     invoke(BUILD/f'diagnostic-{variant}',params,environments['default']))
    for policy,env in environments.items():
        for n in [256,1024,4096]:
            for pair in range(5):
                for variant in (['std','bb'] if pair%2==0 else ['bb','std']):
                    params=['phases',n,300]
                    save('phases',variant,policy,pair,params,invoke(BUILD/f'diagnostic-{variant}',params,env))
    for n in [1024,1048576]:
        for batched in ['false','true']:
            for pair in range(10):
                for variant in (['std','bb'] if pair%2==0 else ['bb','std']):
                    params=['lookup',n,300,batched]
                    save('steady-lookup',variant,'default',pair,params,
                         invoke(BUILD/f'diagnostic-{variant}',params,environments['default']))
            print(f'lookup n={n} batched={batched} done',flush=True)
    record_cpu_times(args.output)
    for variant in ['std','bb']:
        data=BUILD/f'{variant}-pending.perf.data'
        binary=BUILD/f'maps-target-{variant}/release/maps'
        output=subprocess.check_output(['perf','record','-q','-F','99','-e','cpu-clock:u','--call-graph','dwarf,8192',
                                       '-o',str(data),'--',str(binary),'pending','1024','ordered','1500'],text=True)
        save('perf-record',variant,'default',0,[],output.strip())
        report=subprocess.check_output(['perf','report','--stdio','--no-children','--no-inline',
                                        '--call-graph','none','--sort','symbol','--percent-limit','1','-i',str(data)],
                                       text=True,env=dict(os.environ,DEBUGINFOD_URLS=''),timeout=30)
        (args.output/f'{variant}-pending-profile.txt').write_text(report)
    print('DIAGNOSTICS FINISHED',flush=True)


if __name__=='__main__':
    main()
