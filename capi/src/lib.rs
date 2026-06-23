use std::ptr::null_mut;

use errno::{set_errno, Errno};
use libc::{c_int, size_t};
use mappings::MAPPINGS;
use std::ffi::CString;
use std::io::BufReader;
use tempfile::NamedTempFile;
use tikv_jemalloc_ctl::raw;
use util::parse_jeheap;

pub const JP_SUCCESS: c_int = 0;
pub const JP_FAILURE: c_int = -1;

enum Error {
    Io(std::io::Error),
    Mallctl(c_int),
    ParseProfile(),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<tikv_jemalloc_ctl::Error> for Error {
    fn from(err: tikv_jemalloc_ctl::Error) -> Self {
        // SAFETY: `tikv_jemalloc_ctl::Error` is `repr(transparent)` over a nonzero C int.
        let errno = unsafe { std::mem::transmute::<tikv_jemalloc_ctl::Error, c_int>(err) };
        Self::Mallctl(errno)
    }
}

fn dump_pprof_inner() -> Result<Vec<u8>, Error> {
    let f = NamedTempFile::new()?;
    let path = CString::new(f.path().as_os_str().as_encoded_bytes()).expect("temp path is valid");
    // SAFETY: "prof.dump" is documented as being writable and taking a C string as input:
    // http://jemalloc.net/jemalloc.3.html#prof.dump
    unsafe { raw::write(b"prof.dump\0", path.as_ptr()) }?;

    let dump_reader = BufReader::new(f);
    let profile =
        parse_jeheap(dump_reader, MAPPINGS.as_deref()).map_err(|_| Error::ParseProfile())?;
    let pprof = profile.to_pprof(("inuse_space", "bytes"), ("space", "bytes"), None);
    Ok(pprof)
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
#[no_mangle]
pub unsafe extern "C" fn dump_jemalloc_pprof(buf_out: *mut *mut u8, n_out: *mut size_t) -> c_int {
    let buf = match dump_pprof_inner() {
        Ok(buf) => buf,
        Err(Error::Io(e)) if e.raw_os_error().is_some() => {
            set_errno(Errno(e.raw_os_error().expect("checked above")));
            return JP_FAILURE;
        }
        Err(Error::Mallctl(errno)) => {
            set_errno(Errno(errno));
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
