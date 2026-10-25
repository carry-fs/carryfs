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
fn main() {
    let funcs: [(&str, fn(&[u64])->u64);4] = [("data_index",data_index),("assignments",assignments),("updates",updates),("pending",pending)];
    for n in [32,256,1024,4096] {
        for pattern in ["ordered", "shuffled"] {
            let mut keys: Vec<u64> = (0..n).collect();
            if pattern == "shuffled" {
                let mut state = 0x12345678_u64;
                for i in (1..keys.len()).rev() {
                    state ^= state << 13; state ^= state >> 7; state ^= state << 17;
                    keys.swap(i, state as usize % (i+1));
                }
            }
            for (name, func) in funcs {
                if name == "pending" && n > 1024 { continue; }
                let expected = func(&keys);
                let start = Instant::now();
                let mut iterations = 0_u64;
                while start.elapsed() < Duration::from_millis(120) {
                    for _ in 0..8 { assert_eq!(func(black_box(&keys)), expected); }
                    iterations += 8;
                }
                println!("{name}\t{n}\t{pattern}\t{}\t{iterations}\t{expected}", start.elapsed().as_nanos());
            }
        }
    }
}
