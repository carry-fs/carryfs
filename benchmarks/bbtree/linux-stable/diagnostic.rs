// Appended to the archived map functions by diagnose.py. Linux x86-64 only.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct CountAlloc;
static COUNT: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
static ZEROED: AtomicUsize = AtomicUsize::new(0);
static ALIGNED: AtomicUsize = AtomicUsize::new(0);

fn count(layout: Layout, zeroed: bool) {
    if COUNT.load(Ordering::Relaxed) {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        ZEROED.fetch_add(usize::from(zeroed), Ordering::Relaxed);
        ALIGNED.fetch_add(usize::from(layout.align() >= 512), Ordering::Relaxed);
    }
}

// SAFETY: every operation forwards the original pointer and layout to System.
unsafe impl GlobalAlloc for CountAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout, false);
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout, true);
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count(Layout::from_size_align(size, layout.align()).unwrap(), false);
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountAlloc = CountAlloc;

fn keys(n: usize, shuffled: bool) -> Vec<u64> {
    let mut keys: Vec<u64> = (0..n as u64).collect();
    if shuffled {
        let mut state = 0x12345678_u64;
        for i in (1..keys.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            keys.swap(i, state as usize % (i+1));
        }
    }
    keys
}

fn allocation_counts(name: &str, n: usize) {
    let keys = keys(n, false);
    ALLOCS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);
    ZEROED.store(0, Ordering::Relaxed);
    ALIGNED.store(0, Ordering::Relaxed);
    COUNT.store(true, Ordering::Relaxed);
    let checksum = match name {
        "data_index" => data_index(&keys),
        "assignments" => assignments(&keys),
        "updates" => updates(&keys),
        "pending" => pending(&keys),
        _ => panic!("unknown allocation workload"),
    };
    COUNT.store(false, Ordering::Relaxed);
    println!("{name}\t{n}\t{}\t{}\t{}\t{}\t{checksum}",
             ALLOCS.load(Ordering::Relaxed), BYTES.load(Ordering::Relaxed),
             ZEROED.load(Ordering::Relaxed), ALIGNED.load(Ordering::Relaxed));
}

fn pending_phases(n: usize, millis: u64) {
    let keys = keys(n, false);
    let expected = pending(&keys);
    let warmup = Instant::now();
    while warmup.elapsed() < Duration::from_millis(100) {
        assert_eq!(pending(black_box(&keys)), expected);
    }
    let mut times = [0_u128; 4];
    let mut iterations = 0_u64;
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(millis) {
        let t = Instant::now();
        let mut map = Map::new();
        for &k in &keys { map.insert(k*4096, vec![(k%251) as u8;4096]); }
        times[0] += t.elapsed().as_nanos();
        let t = Instant::now();
        let mut out = vec![0;8192];
        let mut checksum = 0;
        for &k in keys.iter().step_by(16) {
            overlay(&map, k*4096, &mut out);
            checksum += out[0] as u64;
            black_box(&out);
        }
        times[1] += t.elapsed().as_nanos();
        let t = Instant::now();
        let grouped = runs(map);
        times[2] += t.elapsed().as_nanos();
        let t = Instant::now();
        for (offset, pieces) in grouped {
            checksum += offset + pieces.iter().map(|p|p.len() as u64).sum::<u64>();
        }
        drop(out);
        times[3] += t.elapsed().as_nanos();
        assert_eq!(black_box(checksum), expected);
        iterations += 1;
    }
    println!("{n}\t{iterations}\t{}\t{}\t{}\t{}\t{expected}",times[0],times[1],times[2],times[3]);
}

fn lookup(n: usize, millis: u64, batched: bool) {
    let keys = keys(n, true);
    let mut map = Map::new();
    for &key in &keys { map.insert(key*2, key+1); }
    let probes: Vec<u64> = keys.iter().take(16384).map(|&k|k*2+(k%2)).collect();
    let expected: u64 = probes.iter().map(|&p|if p%2==0 { p/2+1 } else {0}).sum();
    let pass = || {
        let mut checksum = 0;
        if batched {
            for chunk in probes.chunks_exact(64) {
                #[cfg(feature="bbmap")]
                {
                    let refs = std::array::from_fn::<_,64,_>(|i| &chunk[i]);
                    checksum += map.get_many(refs).into_iter().flatten().copied().sum::<u64>();
                }
                #[cfg(feature="stdmap")]
                { checksum += chunk.iter().filter_map(|key|map.get(key)).copied().sum::<u64>(); }
            }
        } else {
            for key in &probes { checksum += map.get(black_box(key)).copied().unwrap_or(0); }
        }
        black_box(checksum)
    };
    assert_eq!(pass(),expected);
    let warmup = Instant::now();
    while warmup.elapsed()<Duration::from_millis(100) { assert_eq!(pass(),expected); }
    let t=Instant::now();
    let mut iterations=0;
    while t.elapsed()<Duration::from_millis(millis) {
        assert_eq!(pass(),expected);
        iterations+=1;
    }
    println!("{n}\t{batched}\t{}\t{}\t{expected}",t.elapsed().as_nanos(),iterations*probes.len());
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    match args[1].as_str() {
        "alloc" => allocation_counts(&args[2],args[3].parse().unwrap()),
        "phases" => pending_phases(args[2].parse().unwrap(),args[3].parse().unwrap()),
        "lookup" => lookup(args[2].parse().unwrap(),args[3].parse().unwrap(),args[4].parse().unwrap()),
        _ => panic!("unknown diagnostic"),
    }
}
