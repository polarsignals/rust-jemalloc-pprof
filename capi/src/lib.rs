use std::ptr::null_mut;

use errno::{Errno, set_errno};
#[cfg(target_os = "linux")]
use libc::c_char;
#[cfg(target_os = "linux")]
use libc::c_void;
use libc::{c_int, size_t};
#[cfg(target_os = "linux")]
use mappings::MAPPINGS;
#[cfg(target_os = "linux")]
use pprof_util::parse_jeheap;
#[cfg(target_os = "linux")]
use std::ffi::CString;
#[cfg(target_os = "linux")]
use std::io::BufReader;
#[cfg(target_os = "linux")]
use std::mem::size_of_val;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
#[cfg(target_os = "linux")]
use tempfile::NamedTempFile;

pub const JP_SUCCESS: c_int = 0;
pub const JP_FAILURE: c_int = -1;

#[cfg(target_os = "linux")]
#[link(name = "jemalloc")]
unsafe extern "C" {
    // int mallctl(const char *name, void *oldp, size_t *oldlenp, void *newp, size_t newlen);
    fn mallctl(
        name: *const c_char,
        oldp: *mut c_void,
        oldlenp: *mut size_t,
        newp: *mut c_void,
        newlen: size_t,
    ) -> c_int;
}

enum Error {
    Io(std::io::Error),
    #[cfg(target_os = "linux")]
    Mallctl(c_int),
    #[cfg(target_os = "linux")]
    ParseProfile(),
    #[cfg(not(target_os = "linux"))]
    UnsupportedPlatform,
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(target_os = "linux")]
fn dump_pprof_inner() -> Result<Vec<u8>, Error> {
    let f = NamedTempFile::new()?;
    let path = CString::new(f.path().as_os_str().as_bytes().to_vec()).unwrap();
    // SAFETY: "prof.dump" is documented as being writable and taking a C string as input:
    // http://jemalloc.net/jemalloc.3.html#prof.dump
    let pp = (&mut path.as_ptr()) as *mut _ as *mut _;
    let ret = unsafe {
        mallctl(
            b"prof.dump\0" as *const _ as *const c_char,
            null_mut(),
            null_mut(),
            pp,
            size_of_val(&pp),
        )
    };
    if ret != 0 {
        return Err(Error::Mallctl(ret));
    }

    let dump_reader = BufReader::new(f);
    let profile =
        parse_jeheap(dump_reader, MAPPINGS.as_deref()).map_err(|_| Error::ParseProfile())?;
    let pprof = profile.to_pprof(("inuse_space", "bytes"), ("space", "bytes"), None);
    Ok(pprof)
}

#[cfg(not(target_os = "linux"))]
fn dump_pprof_inner() -> Result<Vec<u8>, Error> {
    Err(Error::UnsupportedPlatform)
}

/// Dump the current jemalloc heap profile in pprof format.
///
/// This is intended to be called from C. A buffer is allocated
/// and a pointer to it is stored in `buf_out`; its size is stored in
/// `n_out`. [`JP_FAILURE`] or [`JP_SUCCESS`] is returned according to whether
/// the operation succeeded or failed; an error code is stored in `errno` if it
/// is meaningful to do so.
///
/// If `JP_FAILURE` is returned, the values pointed to by `buf_out` and `n_out`
/// are unspecified.
///
/// # Safety
///
/// You probably don't want to call this from Rust.
/// Use the Rust API instead.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dump_jemalloc_pprof(buf_out: *mut *mut u8, n_out: *mut size_t) -> c_int {
    let buf = match dump_pprof_inner() {
        Ok(buf) => buf,
        Err(Error::Io(e)) if e.raw_os_error().is_some() => {
            set_errno(Errno(e.raw_os_error().unwrap()));
            return JP_FAILURE;
        }
        #[cfg(target_os = "linux")]
        Err(Error::Mallctl(i)) => {
            set_errno(Errno(i));
            return JP_FAILURE;
        }
        // TODO - maybe some of these can have errnos
        Err(_) => {
            return JP_FAILURE;
        }
    };

    // Disable clippy warning.
    // usize is defined to be the same as uintptr_t (AKA have the same representation as a pointer),
    // which is different from size_t, which is the maximum size of an array.
    // This is not usually an issue, except on some platforms like CHERI which store extra information in the pointer.
    // On those platforms, usize will be 128 bits, while size_t is 64 bit.
    #[allow(clippy::useless_conversion)]
    let len: size_t = buf.len().try_into().expect("absurd length");
    let p = if len > 0 {
        // leak is ok, consumer is responsible for freeing
        buf.leak().as_mut_ptr()
    } else {
        null_mut()
    };
    unsafe {
        if !buf_out.is_null() {
            std::ptr::write(buf_out, p);
        }
        if !n_out.is_null() {
            std::ptr::write(n_out, len);
        }
    }
    JP_SUCCESS
}
