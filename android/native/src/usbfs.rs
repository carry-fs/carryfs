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

// ponytail: one timeout for every transfer; a slow SYNCHRONIZE CACHE needs the headroom.
const TIMEOUT_MS: u32 = 20_000;

/// A claimed mass-storage interface. The descriptor stays owned by the
/// Kotlin `UsbDeviceConnection`, which must outlive this value.
pub struct Usbfs {
    fd: RawFd,
    interface: u16,
    endpoint_in: u8,
    endpoint_out: u8,
}

impl Usbfs {
    pub fn new(fd: RawFd, interface: u16, endpoint_in: u8, endpoint_out: u8) -> Self {
        Self {
            fd,
            interface,
            endpoint_in,
            endpoint_out,
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
        self.bulk(self.endpoint_in, data.as_mut_ptr().cast(), data.len())
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
