//! Bounded file-state oracle shared by deterministic tests and the files fuzzer.
use super::*;
use crate::Name;

/// A file's contents, or a symlink's target.
#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    File(Vec<u8>),
    Link(Vec<u8>),
}

type Snapshot = BTreeMap<String, State>;

fn name(index: u8) -> Name {
    Name::parse(&format!("f{:02}", index % 8)).unwrap()
}

fn contents(volume: &mut Volume<FaultDevice>, handle: &Handle) -> Vec<u8> {
    let size = volume.handle_attributes(handle).unwrap().size as usize;
    assert!(size <= (2 << 20), "oracle file bound exceeded");
    let mut bytes = vec![0; size];
    let mut at = 0;
    while at < size {
        let read = volume.read(handle, at as u64, &mut bytes[at..]).unwrap();
        assert!(read > 0, "premature EOF");
        at += read;
    }
    assert_eq!(volume.read(handle, size as u64, &mut [0; 1]).unwrap(), 0);
    bytes
}

fn snapshot(volume: &mut Volume<FaultDevice>) -> Snapshot {
    let root = volume.open_directory(ROOT).unwrap();
    let mut result = BTreeMap::new();
    let mut cursor = None;
    loop {
        let page = volume.read_directory(&root, cursor.as_ref()).unwrap();
        for entry in page.entries {
            let text = String::from_utf8(entry.name.stored().to_vec()).unwrap();
            let state = if entry.kind == ObjectKind::Symlink {
                State::Link(volume.read_symlink(entry.object).unwrap())
            } else {
                let handle = volume.open_object(entry.object, Access::Read).unwrap();
                let bytes = contents(volume, &handle);
                volume.close_handle(handle).unwrap();
                State::File(bytes)
            };
            assert!(result.insert(text, state).is_none());
        }
        cursor = page.cursor;
        if cursor.is_none() {
            break;
        }
    }
    volume.close_handle(root).unwrap();
    result
}

fn write(volume: &mut Volume<FaultDevice>, handle: &Handle, bytes: &mut Vec<u8>, at: u8, len: u8) {
    let offset = [0, 1, 4095, 65_535, 65_536, 131_073][usize::from(at) % 6];
    let length = [1, 4096, 65_536, 1 << 20][usize::from(len) % 4];
    let payload: Vec<u8> = (0..length)
        .map(|i| (i as u8).wrapping_add(len) ^ at)
        .collect();
    assert_eq!(
        volume.write(handle, offset as u64, &payload).unwrap(),
        length
    );
    if bytes.len() < offset + length {
        bytes.resize(offset + length, 0);
    }
    bytes[offset..offset + length].copy_from_slice(&payload);
}

pub(super) fn fuzz(data: &[u8]) {
    let mut volume = Volume::format_at(
        FaultDevice::new(BTreeMap::new(), MountMode::ReadWrite),
        UUID,
        1,
        0,
    )
    .unwrap();
    let deferred = data.first().is_some_and(|b| b & 1 != 0);
    volume.set_deferred_checkpoints(deferred).unwrap();
    let mut names: BTreeMap<String, ObjectId> = BTreeMap::new();
    let mut objects: BTreeMap<ObjectId, State> = BTreeMap::new();
    let mut pins: Vec<(ObjectId, Handle)> = Vec::new();
    // Every permitted crash state is an acknowledged operation prefix since
    // the last explicit sync. Automatic checkpoints may publish a prefix too.
    let mut permitted = vec![Snapshot::new()];
    for &[op, a, b, c] in data.as_chunks::<4>().0.iter().take(64) {
        let source = name(a);
        let text = String::from_utf8(source.stored().to_vec()).unwrap();
        match op % 12 {
            0 => match volume.create_file(ROOT, source) {
                Ok(handle) => {
                    let object = volume.lookup(ROOT, &name(a)).unwrap();
                    assert!(names.insert(text, object).is_none());
                    objects.insert(object, State::File(Vec::new()));
                    volume.close_handle(handle).unwrap();
                }
                Err(Error::NameCollision) => assert!(names.contains_key(&text)),
                Err(error) => panic!("create: {error:?}"),
            },
            1..=3 => {
                if let Some(&object) = names.get(&text)
                    && let State::File(bytes) = objects.get_mut(&object).unwrap()
                {
                    let handle = volume.open_object(object, Access::ReadWrite).unwrap();
                    if op % 12 == 3 {
                        let size = [0, 1, 4095, 65_535, 65_536, 131_073][usize::from(b) % 6];
                        volume.truncate(&handle, size as u64).unwrap();
                        bytes.resize(size, 0);
                    } else {
                        write(&mut volume, &handle, bytes, b, c);
                    }
                    volume.close_handle(handle).unwrap();
                }
            }
            4 => {
                if let Some(&object) = names.get(&text) {
                    let target = name(b);
                    let to = String::from_utf8(target.stored().to_vec()).unwrap();
                    volume
                        .rename(ROOT, &source, ROOT, target, Replace::Allowed)
                        .unwrap();
                    names.remove(&text);
                    names.insert(to, object);
                }
            }
            5 => {
                if names.remove(&text).is_some() {
                    let result = if a & 8 == 0 {
                        volume.remove(ROOT, &source, RemoveKind::File)
                    } else {
                        volume.remove_with(ROOT, &source, RemoveKind::File, |stored| {
                            assert_eq!(stored, source.stored());
                            Ok(())
                        })
                    };
                    result.unwrap();
                }
            }
            6 => {
                if pins.len() < 8
                    && let Some(&object) = names.get(&text)
                    && matches!(objects[&object], State::File(_))
                {
                    pins.push((
                        object,
                        volume.open_object(object, Access::ReadWrite).unwrap(),
                    ));
                }
            }
            7 => {
                if !pins.is_empty() {
                    let (object, handle) = &pins[usize::from(a) % pins.len()];
                    let Some(State::File(bytes)) = objects.get_mut(object) else {
                        unreachable!("only files are pinned");
                    };
                    write(&mut volume, handle, bytes, b, c);
                }
            }
            8 => {
                if !pins.is_empty() {
                    let (_, handle) = pins.remove(usize::from(a) % pins.len());
                    volume.close_handle(handle).unwrap();
                }
            }
            9 => {
                volume.sync().unwrap();
                permitted.clear();
            }
            10 => {
                let media = volume.device.as_ref().unwrap().media.clone();
                drop(volume);
                media.borrow_mut().crash();
                let durable = media.borrow().durable.clone();
                let mut reader =
                    Volume::open(FaultDevice::new(durable.clone(), MountMode::ReadOnly)).unwrap();
                reader.check().unwrap();
                let recovered = snapshot(&mut reader);
                assert!(
                    permitted.contains(&recovered),
                    "recovery is not an acknowledged prefix"
                );
                volume = Volume::open(FaultDevice::new(durable, MountMode::ReadWrite)).unwrap();
                volume.set_deferred_checkpoints(deferred).unwrap();
                pins.clear();
                names.clear();
                objects.clear();
                for (text, state) in recovered {
                    let object = volume.lookup(ROOT, &Name::parse(&text).unwrap()).unwrap();
                    names.insert(text, object);
                    objects.insert(object, state);
                }
                permitted.clear();
            }
            11 => {
                let target = format!("../t{b}/{c}").into_bytes();
                match volume.create_symlink(ROOT, source, &target) {
                    Ok(object) => {
                        assert!(names.insert(text, object).is_none());
                        objects.insert(object, State::Link(target));
                    }
                    Err(Error::NameCollision) => assert!(names.contains_key(&text)),
                    Err(error) => panic!("symlink: {error:?}"),
                }
            }
            _ => unreachable!("op % 12"),
        }
        let expected: Snapshot = names
            .iter()
            .map(|(text, id)| (text.clone(), objects[id].clone()))
            .collect();
        assert_eq!(snapshot(&mut volume), expected);
        for (object, handle) in &pins {
            assert_eq!(
                State::File(contents(&mut volume, handle)),
                objects[object],
                "pinned contents"
            );
        }
        objects
            .retain(|id, _| names.values().any(|n| n == id) || pins.iter().any(|(p, _)| p == id));
        if !deferred {
            permitted.clear();
        }
        if permitted.last() != Some(&expected) {
            permitted.push(expected);
        }
    }
    for (_, handle) in pins {
        volume.close_handle(handle).unwrap();
    }
    volume.sync().unwrap();
    let expected = snapshot(&mut volume);
    let media = volume.device.as_ref().unwrap().media.clone();
    drop(volume);
    media.borrow_mut().crash();
    let mut reader = Volume::open(FaultDevice::new(
        media.borrow().durable.clone(),
        MountMode::ReadOnly,
    ))
    .unwrap();
    reader.check().unwrap();
    assert_eq!(snapshot(&mut reader), expected, "sync lost file state");
}
