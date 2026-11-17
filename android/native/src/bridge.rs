//! JNI entry points for `com.carryfs.android.Native`. Errors are thrown as
//! `FileNotFoundException` for a missing object and `IOException` otherwise.

use std::fs::OpenOptions;
use std::ptr::null_mut;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use carryfs_core::Error;
use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jboolean, jbyteArray, jint, jlong, jlongArray, jstring};

use crate::disk::ImageFile;
use crate::scsi::Disk;
use crate::session::{Entry, Session};
use crate::usbfs::Usbfs;

// The maintenance thread starts with the session. It waits for this
// initializer to finish before its first lock, and sleeps before that anyway.
static SESSION: LazyLock<Mutex<Session>> = LazyLock::new(|| {
    thread::spawn(|| {
        loop {
            thread::sleep(Duration::from_secs(1));
            for error in session().maintain() {
                log(&format!("publishing changes failed: {error}"));
            }
        }
    });
    Mutex::default()
});

fn session() -> MutexGuard<'static, Session> {
    // A panic in an entry point aborts the app, so only a panicking
    // maintenance step could poison the lock; failing loudly then is right.
    SESSION.lock().expect("session lock poisoned")
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
        let volumes = session().mount(drive, Arc::new(Mutex::new(disk)))?;
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
        let volumes = session().mount(drive, Arc::new(Mutex::new(image)))?;
        long_array(env, &volumes)
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_eject<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    drive: jlong,
) {
    call(&mut env, (), |_| Ok(session().eject(drive)?));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_detach<'local>(
    _: JNIEnv<'local>,
    _: JClass<'local>,
    drive: jlong,
) {
    session().detach(drive);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_label<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
) -> jstring {
    call(&mut env, null_mut(), |env| {
        let label = session().label(volume)?;
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
        let (available, total) = session().space(volume)?;
        long_array(env, &[available as i64, total as i64])
    })
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_readOnly<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
) -> jboolean {
    call(&mut env, 1, |_| Ok(session().read_only(volume)?.into()))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_needsUpgrade<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
) -> jboolean {
    call(&mut env, 0, |_| Ok(session().needs_upgrade(volume)?.into()))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_upgrade<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
) {
    call(&mut env, (), |_| Ok(session().upgrade(volume)?));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_list<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
    directory: jlong,
) -> jbyteArray {
    call(&mut env, null_mut(), |env| {
        let entries = session().list(volume, directory)?;
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
        let entry = session().stat(volume, id)?;
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
        Ok(session().create(volume, parent, &name, directory != 0)?)
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
        Ok(session().rename(volume, id, &name)?)
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
        Ok(session().move_to(volume, id, from, to)?)
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_delete<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
    id: jlong,
) {
    call(&mut env, (), |_| Ok(session().delete(volume, id)?));
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
        Ok(session().open(volume, id, write != 0, truncate != 0)?)
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
        let read = session().read(volume, file, offset, &mut bytes)?;
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
        Ok(session().write(volume, file, offset, data)?)
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_size<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
    file: jlong,
) -> jlong {
    call(&mut env, -1, |_| Ok(session().size(volume, file)? as jlong))
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_sync<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
) {
    call(&mut env, (), |_| Ok(session().sync(volume)?));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_carryfs_android_Native_close<'local>(
    mut env: JNIEnv<'local>,
    _: JClass<'local>,
    volume: jlong,
    file: jlong,
) {
    call(&mut env, (), |_| Ok(session().close(volume, file)?));
}
