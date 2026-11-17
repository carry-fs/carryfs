//! CarryFS for Android: USB mass storage driven from user space, and the
//! JNI surface the app's documents provider calls.
#![cfg(unix)]

mod bridge;
mod disk;
mod scsi;
mod session;
mod usbfs;
