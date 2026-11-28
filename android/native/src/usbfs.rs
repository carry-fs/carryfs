//! The usbfs descriptor that Android's `UsbDeviceConnection` opens, driven
//! with the same ioctls libusb uses on Android.

use std::io;
use std::os::fd::RawFd;
use std::ptr;

use crate::scsi::{Endpoint, Transport};

#[repr(C)]
struct BulkTransfer {
    endpoint: libc::c_uint,
    len: libc::c_uint,
    timeout_ms: libc::c_uint,
    data: *mut libc::c_void,
}

#[repr(C)]
struct ControlTransfer {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
    timeout_ms: u32,
    data: *mut libc::c_void,
}

// linux/usbdevice_fs.h
const fn ioc(direction: u32, number: u32, size: usize) -> u32 {
    (direction << 30) | ((size as u32) << 16) | ((b'U' as u32) << 8) | number
}
const USBDEVFS_CONTROL: u32 = ioc(3, 0, size_of::<ControlTransfer>());
const USBDEVFS_BULK: u32 = ioc(3, 2, size_of::<BulkTransfer>());
const USBDEVFS_CLEAR_HALT: u32 = ioc(2, 21, size_of::<libc::c_uint>());
const USBDEVFS_RESET: u32 = ioc(0, 20, 0);

/// Reset the whole device. The kernel then re-probes it as if it were replugged.
pub fn reset_device(fd: RawFd) -> io::Result<()> {
    // SAFETY: USBDEVFS_RESET takes no argument.
    if unsafe { libc::ioctl(fd, USBDEVFS_RESET as _) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ponytail: one timeout for every transfer; a slow SYNCHRONIZE CACHE needs the headroom.
const TIMEOUT_MS: u32 = 20_000;
const FALLBACK_READ_BYTES: usize = 16 << 10;

/// A claimed mass-storage interface. The descriptor stays owned by the
/// Kotlin `UsbDeviceConnection`, which must outlive this value.
pub struct Usbfs {
    fd: RawFd,
    interface: u16,
    endpoint_in: u8,
    endpoint_out: u8,
    bulk_in_limit: usize,
}

impl Usbfs {
    pub fn new(fd: RawFd, interface: u16, endpoint_in: u8, endpoint_out: u8) -> Self {
        Self {
            fd,
            interface,
            endpoint_in,
            endpoint_out,
            bulk_in_limit: usize::MAX,
        }
    }

    fn bulk(&self, endpoint: u8, data: *mut libc::c_void, len: usize) -> io::Result<usize> {
        let mut transfer = BulkTransfer {
            endpoint: endpoint.into(),
            len: u32::try_from(len).map_err(io::Error::other)?,
            timeout_ms: TIMEOUT_MS,
            data,
        };
        // SAFETY: `transfer` matches the kernel's usbdevfs_bulktransfer, and
        // `data` points to `len` bytes that live for the call. For an OUT
        // endpoint the kernel only reads them.
        let moved = unsafe { libc::ioctl(self.fd, USBDEVFS_BULK as _, &mut transfer) };
        if moved < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(moved as usize)
    }

    fn ioctl<T>(&self, request: u32, argument: &mut T) -> io::Result<()> {
        // SAFETY: callers pass the argument type the request declares.
        if unsafe { libc::ioctl(self.fd, request as _, argument as *mut T) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Transport for Usbfs {
    fn bulk_out(&mut self, data: &[u8]) -> io::Result<usize> {
        self.bulk(
            self.endpoint_out,
            data.as_ptr().cast_mut().cast(),
            data.len(),
        )
    }

    fn bulk_in(&mut self, data: &mut [u8]) -> io::Result<usize> {
        let mut limit = self.bulk_in_limit;
        let result = read_bulk(data, &mut limit, |chunk| {
            self.bulk(self.endpoint_in, chunk.as_mut_ptr().cast(), chunk.len())
        });
        self.bulk_in_limit = limit;
        result
    }

    fn clear_halt(&mut self, endpoint: Endpoint) -> io::Result<()> {
        let mut address: libc::c_uint = match endpoint {
            Endpoint::In => self.endpoint_in,
            Endpoint::Out => self.endpoint_out,
        }
        .into();
        self.ioctl(USBDEVFS_CLEAR_HALT, &mut address)
    }

    fn reset(&mut self) -> io::Result<()> {
        let mut request = ControlTransfer {
            request_type: 0x21,
            request: 0xff,
            value: 0,
            index: self.interface,
            length: 0,
            timeout_ms: TIMEOUT_MS,
            data: ptr::null_mut(),
        };
        self.ioctl(USBDEVFS_CONTROL, &mut request)
    }
}

fn read_bulk(
    data: &mut [u8],
    limit: &mut usize,
    mut transfer: impl FnMut(&mut [u8]) -> io::Result<usize>,
) -> io::Result<usize> {
    let mut moved = 0;
    loop {
        let len = (data.len() - moved).min(*limit);
        match transfer(&mut data[moved..moved + len]) {
            Ok(read) => {
                moved += read;
                if read < len || moved == data.len() {
                    return Ok(moved);
                }
            }
            // usbfs reports ENOMEM before submission, so no packet was consumed.
            Err(error)
                if error.raw_os_error() == Some(libc::ENOMEM) && len > FALLBACK_READ_BYTES =>
            {
                *limit = FALLBACK_READ_BYTES;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_failure_retries_the_same_bytes_in_smaller_transfers() {
        let mut data = [0; 4 * FALLBACK_READ_BYTES];
        let mut limit = usize::MAX;
        let mut calls = Vec::new();
        let mut next = 1;
        let read = read_bulk(&mut data, &mut limit, |chunk| {
            calls.push(chunk.len());
            if chunk.len() > FALLBACK_READ_BYTES {
                return Err(io::Error::from_raw_os_error(libc::ENOMEM));
            }
            chunk.fill(next);
            next += 1;
            Ok(chunk.len())
        })
        .unwrap();
        assert_eq!(limit, FALLBACK_READ_BYTES);
        assert_eq!(read, data.len());
        assert_eq!(calls, [data.len(), limit, limit, limit, limit]);
        for (index, chunk) in data.chunks_exact(limit).enumerate() {
            assert!(chunk.iter().all(|&byte| byte == index as u8 + 1));
        }
    }

    #[test]
    fn short_transfer_ends_the_data_phase() {
        let mut data = [0; 3 * FALLBACK_READ_BYTES];
        let mut limit = FALLBACK_READ_BYTES;
        let mut calls = 0;
        let read = read_bulk(&mut data, &mut limit, |chunk| {
            calls += 1;
            assert_eq!(chunk.len(), FALLBACK_READ_BYTES);
            assert!(calls <= 2);
            Ok(if calls == 1 { chunk.len() } else { 7 })
        })
        .unwrap();
        assert_eq!(read, FALLBACK_READ_BYTES + 7);
    }
}
