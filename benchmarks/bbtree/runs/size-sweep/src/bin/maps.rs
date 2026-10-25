#[cfg(feature = "stdmap")]
use std::collections::BTreeMap as Map;
#[cfg(feature = "bbmap")]
use bbtree::unsync::BBTreeMap as Map;
use std::{hint::black_box, time::{Duration, Instant}};

fn data_index(keys: &[u64]) -> u64 {
    let mut map = Map::new();
    for (i, &key) in keys.iter().enumerate() {
        if !map.contains_key(&key) { map.insert(key, i); }
    }
    let mut sum = 0;
    for &first in keys.iter().step_by(16) {
        sum += map.range(first..first+16).count() as u64;
        for (&key, &at) in map.range(first..first+16) { sum += key + at as u64; }
    }
    for key in keys.iter().rev().take(keys.len()/4) { sum += map.remove(key).unwrap() as u64; }
    black_box(sum)
}
fn assignments(keys: &[u64]) -> u64 {
    let mid = keys.len()/2;
    let mut map: Map<usize, u64> = keys[..mid].iter().map(|&k| (k as usize, k+8192)).collect();
    for &key in &keys[mid..] { black_box(map.contains_key(&(key as usize))); }
    map.extend(keys[mid..].iter().map(|&k| (k as usize, k+8192)));
    let mut sum = 0;
    for (id, block) in &map { sum += *block + *map.get(id).unwrap(); }
    black_box(sum)
}
fn updates(keys: &[u64]) -> u64 {
    let mut map = Map::new();
    for &k in keys {
        let mut key = vec![3];
        key.extend_from_slice(&0_u64.to_be_bytes());
        key.extend_from_slice(&k.to_be_bytes());
        map.insert(key, Some(vec![(k%251) as u8;32]));
    }
    for &k in keys.iter().step_by(4) {
        let mut key = vec![3]; key.extend_from_slice(&0_u64.to_be_bytes());
        key.extend_from_slice(&k.to_be_bytes());
        map.insert(key, None);
    }
    let mut sum = 0;
    for (key, value) in &map { sum += key[16] as u64 + value.as_ref().map_or(0, |v| v[0] as u64); }
    black_box(sum)
}
const MAX_RUN: usize = 1 << 20;
fn overlay(pending: &Map<u64, Vec<u8>>, offset: u64, out: &mut [u8]) {
    let end = offset + out.len() as u64;
    for (&at, data) in pending.range(..end).rev() {
        let data_end = at + data.len() as u64;
        if data_end <= offset {
            break;
        }
        let (low, high) = (at.max(offset), data_end.min(end));
        out[(low - offset) as usize..(high - offset) as usize]
            .copy_from_slice(&data[(low - at) as usize..(high - at) as usize]);
    }
}

/// Group writes that end where the next begins into runs of at most `MAX_RUN`,
/// keeping each write's buffer: the writer sends a run as one `pwritev`, so
/// nothing is copied into a merged buffer. A write longer than that is sent
/// alone.
fn runs(pending: Map<u64, Vec<u8>>) -> Vec<(u64, Vec<Vec<u8>>)> {
    let mut runs: Vec<(u64, Vec<Vec<u8>>, usize)> = Vec::new();
    for (offset, data) in pending {
        match runs.last_mut() {
            Some((start, pieces, len))
                if *start + *len as u64 == offset && *len + data.len() <= MAX_RUN =>
            {
                *len += data.len();
                pieces.push(data);
            }
            _ => {
                let len = data.len();
                runs.push((offset, vec![data], len));
            }
        }
    }
    runs.into_iter()
        .map(|(offset, pieces, _)| (offset, pieces))
        .collect()
}

fn pending(keys: &[u64]) -> u64 {
    // Include buffer allocation/destruction; use the actual adapter overlay/run code.
    let mut map = Map::new();
    for &k in keys { map.insert(k*4096, vec![(k%251) as u8; 4096]); }
    let mut out = vec![0;8192];
    let mut sum = 0;
    for &k in keys.iter().step_by(16) {
        overlay(&map, k*4096, &mut out);
        sum += out[0] as u64;
        black_box(&out);
    }
    for (offset, pieces) in runs(map) { sum += offset + pieces.iter().map(|p|p.len() as u64).sum::<u64>(); }
    black_box(sum)
}
#[repr(C)]
struct Timespec { seconds: i64, nanos: i64 }
unsafe extern "C" {
    fn clock_gettime(clock: i32, out: *mut Timespec) -> i32;
    fn pthread_set_qos_class_self_np(class: u32, priority: i32) -> i32;
}
fn cpu_ns() -> u128 {
    let mut time = Timespec { seconds: 0, nanos: 0 };
    // SAFETY: macOS clock 12 is process CPU time; out points to a valid timespec.
    let status = unsafe { clock_gettime(12, &mut time) };
    assert_eq!(status, 0, "process CPU clock failed: {}", std::io::Error::last_os_error());
    assert!(time.seconds >= 0 && (0..1_000_000_000).contains(&time.nanos));
    time.seconds as u128 * 1_000_000_000 + time.nanos as u128
}
fn main() {
    // SAFETY: request user-initiated QoS for this thread using SDK enum 0x19.
    let status = unsafe { pthread_set_qos_class_self_np(0x19, 0) };
    assert_eq!(status, 0, "thread QoS request failed with errno {status}");

    let args: Vec<_> = std::env::args().collect();
    let name = &args[1];
    let n: usize = args[2].parse().unwrap();
    let pattern = &args[3];
    let millis: u64 = args[4].parse().unwrap();
    assert!(n > 0 && n <= 6_291_456);
    let func: fn(&[u64])->u64 = match name.as_str() {
        "data_index" => data_index, "assignments" => assignments,
        "updates" => updates, "pending" => pending, _ => panic!("unknown map workload"),
    };
    let mut keys: Vec<u64> = (0..n as u64).collect();
    if pattern == "shuffled" {
        let mut state = 0x12345678_u64;
        for i in (1..keys.len()).rev() {
            state ^= state << 13; state ^= state >> 7; state ^= state << 17;
            keys.swap(i, state as usize % (i+1));
        }
    } else { assert_eq!(pattern, "ordered"); }
    let expected = func(&keys);
    let warmup = Instant::now();
    while warmup.elapsed() < Duration::from_millis(100) {
        assert_eq!(func(black_box(&keys)), expected);
    }
    let cpu_start = cpu_ns();
    let start = Instant::now();
    let mut iterations = 0_u64;
    let batch = (4096 / n).clamp(1, 64);
    while start.elapsed() < Duration::from_millis(millis) {
        for _ in 0..batch { assert_eq!(func(black_box(&keys)), expected); }
        iterations += batch as u64;
    }
    let elapsed = start.elapsed().as_nanos();
    let cpu_elapsed = cpu_ns() - cpu_start;
    assert!(cpu_elapsed > 0);
    println!("{name}\t{n}\t{pattern}\t{elapsed}\t{iterations}\t{expected}\t{cpu_elapsed}");
}
