//! JNI entry points for `com.carryfs.android.Native`. Errors are thrown as
//! `FileNotFoundException` for a missing object and `IOException` otherwise.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::ptr::null_mut;
use std::sync::{Arc, LazyLock, Mutex};
use std::thread;
use std::time::Duration;

use carryfs_core::Error;
use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jboolean, jbyteArray, jint, jlong, jlongArray, jstring};

use crate::disk::{ImageFile, SharedDisk};
use crate::scsi::Disk;
use crate::session::{Entry, Session};
use crate::usbfs::Usbfs;

type Drive = Arc<Mutex<Session>>;

#[derive(Default)]
struct Registry {
    drives: BTreeMap<i64, Drive>,
    volumes: BTreeMap<i64, i64>,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(Mutex::default);

fn mount(drive: i64, disk: SharedDisk) -> Result<Vec<i64>, Error> {
    let session = Drive::default();
    {
        let mut registry = REGISTRY.lock().map_err(|_| Error::Faulted)?;
        if registry.drives.contains_key(&drive) {
            return Err(Error::Busy);
        }
        registry.drives.insert(drive, session.clone());
    }
    let mut state = session.lock().map_err(|_| Error::Faulted)?;
    let mounted = state.mount(drive, disk);
    let mut registry = REGISTRY.lock().map_err(|_| Error::Faulted)?;
    if !registry
        .drives
        .get(&drive)
        .is_some_and(|current| Arc::ptr_eq(current, &session))
    {
        return Err(Error::Ejected);
    }
    match mounted {
        Ok(volumes) if !volumes.is_empty() => {
            registry
                .volumes
                .extend(volumes.iter().map(|&id| (id, drive)));
            let weak = Arc::downgrade(&session);
            thread::spawn(move || {
                loop {
                    thread::sleep(Duration::from_secs(1));
                    let Some(session) = weak.upgrade() else {
                        return;
                    };
                    let Ok(mut state) = session.lock() else {
                        return;
                    };
                    for error in state.maintain() {
                        log(&format!("publishing changes failed: {error}"));
                    }
                }
            });
            Ok(volumes)
        }
        other => {
            registry.drives.remove(&drive);
            other
        }
    }
}

fn with_volume<T>(
    volume: i64,
    op: impl FnOnce(&mut Session) -> Result<T, Error>,
) -> Result<T, Error> {
    let session = {
        let registry = REGISTRY.lock().map_err(|_| Error::Faulted)?;
        registry
            .volumes
            .get(&volume)
            .and_then(|drive| registry.drives.get(drive))
            .cloned()
            .ok_or(Error::Ejected)?
    };
    let mut state = session.lock().map_err(|_| Error::Faulted)?;
    op(&mut state)
}

fn release(drive: i64, eject: bool) -> Result<(), Error> {
    let session = REGISTRY
        .lock()
        .map_err(|_| Error::Faulted)?
        .drives
        .get(&drive)
        .cloned();
    let Some(session) = session else {
        return Ok(());
    };
    let mut state = session.lock().map_err(|_| Error::Faulted)?;
    if eject {
        state.eject(drive)?;
    } else {
        state.detach(drive);
    }
    let mut registry = REGISTRY.lock().map_err(|_| Error::Faulted)?;
    if registry
        .drives
        .get(&drive)
        .is_some_and(|current| Arc::ptr_eq(current, &session))
    {
        registry.drives.remove(&drive);
        registry.volumes.retain(|_, key| *key != drive);
    }
    Ok(())
}

enum Failure {
    Core(Error),
    Jni(jni::errors::Error),
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Failure::Core(error)
    }
}

impl From<jni::errors::Error> for Failure {
    fn from(error: jni::errors::Error) -> Self {
        Failure::Jni(error)
    }
}

type Outcome<T> = Result<T, Failure>;

/// Run `op`, turning a failure into a pending Java exception and `fallback`.
fn call<'local, T>(
    env: &mut JNIEnv<'local>,
    fallback: T,
    op: impl FnOnce(&mut JNIEnv<'local>) -> Outcome<T>,
) -> T {
    match op(env) {
        Ok(value) => value,
        Err(failure) => {
            // A JNI failure usually has its Java exception pending already.
            if !env.exception_check().unwrap_or(true) {
                let (class, message) = match failure {
                    Failure::Core(Error::NotFound) => (
                        "java/io/FileNotFoundException",
                        "no such file or folder".to_owned(),
                    ),
                    Failure::Core(error) => ("java/io/IOException", error.to_string()),
                    Failure::Jni(error) => ("java/lang/IllegalStateException", error.to_string()),
                };
                // If throwing fails, the JVM has its own error pending instead.
                let _ = env.throw_new(class, message);
            }
            fallback
        }
    }
}

fn long_array(env: &mut JNIEnv<'_>, values: &[i64]) -> Outcome<jlongArray> {
    let array = env.new_long_array(values.len() as i32)?;
    env.set_long_array_region(&array, 0, values)?;
    Ok(array.into_raw())
}

fn small<T: TryFrom<i32>>(value: i32, what: &'static str) -> Result<T, Error> {
    T::try_from(value).map_err(|_| Error::InvalidInput(what))
}

/// Entries as Kotlin's `Native.decode` reads them, little-endian: id i64,
/// parent i64, directory u8, size i64, modified ms i64, name length i32,
/// UTF-8 name.
fn encode(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in entries {
        out.extend_from_slice(&entry.id.to_le_bytes());
        out.extend_from_slice(&entry.parent.to_le_bytes());
        out.push(entry.directory.into());
        out.extend_from_slice(&entry.size.to_le_bytes());
        out.extend_from_slice(&entry.modified_ms.to_le_bytes());
        out.extend_from_slice(&(entry.name.len() as u32).to_le_bytes());
        out.extend_from_slice(entry.name.as_bytes());
    }
    out
}

#[cfg(target_os = "android")]
fn log(message: &str) {
    use std::ffi::{CString, c_char, c_int};
    #[link(name = "log")]
    unsafe extern "C" {
        fn __android_log_write(priority: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }
    const ERROR: c_int = 6;
    let text = CString::new(message.replace('\0', " ")).unwrap_or_default();
    // SAFETY: both strings are NUL-terminated and live for the call.
    unsafe { __android_log_write(ERROR, c"CarryFS".as_ptr(), text.as_ptr()) };
}

#[cfg(not(target_os = "android"))]
fn log(message: &str) {
    eprintln!("carryfs: {message}");
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_mountUsb<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    drive: jlong,
    fd: jint,
    interface: jint,
    endpoint_in: jint,
    endpoint_out: jint,
) -> jlongArray {
    call(&mut env, null_mut(), |env| {
        let usb = Usbfs::new(
            fd,
            small(interface, "USB interface is out of range")?,
            small(endpoint_in, "USB endpoint is out of range")?,
            small(endpoint_out, "USB endpoint is out of range")?,
        );
        let disk = Disk::open(usb).map_err(|source| Error::Io {
            operation: "open USB disk",
            source,
        })?;
        let volumes = mount(drive, Arc::new(Mutex::new(disk)))?;
        long_array(env, &volumes)
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_mountImage<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    drive: jlong,
    path: JString<'local>,
) -> jlongArray {
    call(&mut env, null_mut(), |env| {
        let path: String = env.get_string(&path)?.into();
        let io = |operation| move |source| Error::Io { operation, source };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(io("open disk image"))?;
        let image = ImageFile::new(file).map_err(io("inspect disk image"))?;
        let volumes = mount(drive, Arc::new(Mutex::new(image)))?;
        long_array(env, &volumes)
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_eject<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    drive: jlong,
) {
    call(&mut env, (), |_| Ok(release(drive, true)?));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_detach<'local>(
    _: JNIEnv<'local>,
    _: JClass<'local>,
    drive: jlong,
) {
    let _ = release(drive, false);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_label<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
) -> jstring {
    call(&mut env, null_mut(), |env| {
        let label = with_volume(volume, |session| session.label(volume))?;
        Ok(env.new_string(label)?.into_raw())
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_space<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
) -> jlongArray {
    call(&mut env, null_mut(), |env| {
        let (available, total) = with_volume(volume, |session| session.space(volume))?;
        long_array(env, &[available as i64, total as i64])
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_readOnly<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
) -> jboolean {
    call(&mut env, 1, |_| {
        Ok(with_volume(volume, |session| session.read_only(volume))?.into())
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_needsUpgrade<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
) -> jboolean {
    call(&mut env, 0, |_| {
        Ok(with_volume(volume, |session| session.needs_upgrade(volume))?.into())
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_upgrade<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
) {
    call(&mut env, (), |_| {
        Ok(with_volume(volume, |session| session.upgrade(volume))?)
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_list<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
    directory: jlong,
) -> jbyteArray {
    call(&mut env, null_mut(), |env| {
        let entries = with_volume(volume, |session| session.list(volume, directory))?;
        Ok(env.byte_array_from_slice(&encode(&entries))?.into_raw())
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_stat<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
    id: jlong,
) -> jbyteArray {
    call(&mut env, null_mut(), |env| {
        let entry = with_volume(volume, |session| session.stat(volume, id))?;
        Ok(env.byte_array_from_slice(&encode(&[entry]))?.into_raw())
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_create<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
    parent: jlong,
    name: JString<'local>,
    directory: jboolean,
) -> jlong {
    call(&mut env, 0, |env| {
        let name: String = env.get_string(&name)?.into();
        Ok(with_volume(volume, |session| {
            session.create(volume, parent, &name, directory != 0)
        })?)
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_rename<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
    id: jlong,
    name: JString<'local>,
) {
    call(&mut env, (), |env| {
        let name: String = env.get_string(&name)?.into();
        Ok(with_volume(volume, |session| {
            session.rename(volume, id, &name)
        })?)
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_move<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
    id: jlong,
    from: jlong,
    to: jlong,
) {
    call(&mut env, (), |_| {
        Ok(with_volume(volume, |session| {
            session.move_to(volume, id, from, to)
        })?)
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_delete<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
    id: jlong,
) {
    call(&mut env, (), |_| {
        Ok(with_volume(volume, |session| session.delete(volume, id))?)
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_open<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
    id: jlong,
    write: jboolean,
    truncate: jboolean,
) -> jlong {
    call(&mut env, 0, |_| {
        Ok(with_volume(volume, |session| {
            session.open(volume, id, write != 0, truncate != 0)
        })?)
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_read<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
    file: jlong,
    offset: jlong,
    size: jint,
    buffer: JByteArray<'local>,
) -> jint {
    call(&mut env, -1, |env| {
        let offset = u64::try_from(offset).map_err(|_| Error::InvalidInput("negative offset"))?;
        let mut bytes = vec![0; small::<usize>(size, "negative size")?];
        let read = with_volume(volume, |session| {
            session.read(volume, file, offset, &mut bytes)
        })?;
        let signed: Vec<i8> = bytes[..read].iter().map(|&byte| byte as i8).collect();
        env.set_byte_array_region(&buffer, 0, &signed)?;
        Ok(read as jint)
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_write<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
    file: jlong,
    offset: jlong,
    size: jint,
    buffer: JByteArray<'local>,
) {
    call(&mut env, (), |env| {
        let offset = u64::try_from(offset).map_err(|_| Error::InvalidInput("negative offset"))?;
        let size = small::<usize>(size, "negative size")?;
        let bytes = env.convert_byte_array(&buffer)?;
        let data = bytes
            .get(..size)
            .ok_or(Error::InvalidInput("size is larger than the buffer"))?;
        Ok(with_volume(volume, |session| {
            session.write(volume, file, offset, data)
        })?)
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_size<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
    file: jlong,
) -> jlong {
    call(&mut env, -1, |_| {
        Ok(with_volume(volume, |session| session.size(volume, file))? as jlong)
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_sync<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
) {
    call(&mut env, (), |_| {
        Ok(with_volume(volume, |session| session.sync(volume))?)
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_close<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
    file: jlong,
) {
    call(&mut env, (), |_| {
        match with_volume(volume, |session| session.close(volume, file)) {
            Ok(()) | Err(Error::Ejected) => Ok(()),
            Err(error) => Err(error.into()),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::tests::{MIB, format, usb_disk};
    use std::sync::mpsc;

    #[test]
    fn a_busy_drive_does_not_block_another_drive() {
        let mount_image = |key| {
            let disk = usb_disk(300 * MIB);
            format(&disk, 0, 300 * MIB);
            mount(key, disk).unwrap()[0]
        };
        let first = mount_image(1);
        let second = mount_image(2);
        let (busy, started) = mpsc::channel();
        let (release_first, unblock) = mpsc::channel();
        let worker = thread::spawn(move || {
            with_volume(first, |_| {
                busy.send(()).unwrap();
                unblock.recv().unwrap();
                Ok(())
            })
            .unwrap();
        });
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let (sender, ready) = mpsc::channel();
        let other = thread::spawn(move || {
            sender
                .send(with_volume(second, |session| session.label(second)))
                .unwrap();
        });
        let result = ready.recv_timeout(Duration::from_secs(2));
        release_first.send(()).unwrap();
        worker.join().unwrap();
        other.join().unwrap();
        assert!(result.unwrap().is_ok());
        release(1, false).unwrap();
        release(2, false).unwrap();
    }
}
