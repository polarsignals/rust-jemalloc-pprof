// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License in the LICENSE file at the
// root of this repository, or online at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Linux-specific process introspection.

//! Utility crate to extract information about the running process.
//!
//! Currently only works on Linux.
use std::path::PathBuf;

use once_cell::sync::Lazy;
use tracing::error;

use util::{BuildId, Mapping};

#[cfg(not(target_pointer_width = "64"))]
compile_error!("this module only supports 64-bit targets");

#[cfg(target_os = "linux")]
use linux::collect_shared_objects;

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::str::FromStr;

    use anyhow::Context;
    use libc::{
        c_int, c_void, dl_iterate_phdr, dl_phdr_info, size_t, Elf64_Word, PT_LOAD, PT_NOTE,
    };

    use util::{BuildId, CastFrom};

    use crate::LoadedSegment;

    use super::SharedObject;

    /// Collects information about all shared objects loaded into the current
    /// process, including the main program binary as well as all dynamically loaded
    /// libraries. Intended to be useful for profilers, who can use this information
    /// to symbolize stack traces offline.
    ///
    /// Uses `dl_iterate_phdr` to walk all shared objects and extract the wanted
    /// information from their program headers.
    ///
    /// SAFETY: This function is written in a hilariously unsafe way: it involves
    /// following pointers to random parts of memory, and then assuming that
    /// particular structures can be found there. However, it was written by
    /// carefully reading `man dl_iterate_phdr` and `man elf`, and is thus intended
    /// to be relatively safe for callers to use. Assuming I haven't written any
    /// bugs (and that the documentation is correct), the only known safety
    /// requirements are:
    ///
    /// (1) The running binary must be in ELF format and running on Linux.
    pub unsafe fn collect_shared_objects() -> Result<Vec<SharedObject>, anyhow::Error> {
        let mut state = CallbackState {
            result: Ok(Vec::new()),
        };
        let state_ptr = std::ptr::addr_of_mut!(state).cast();

        // SAFETY: `dl_iterate_phdr` has no documented restrictions on when
        // it can be called.
        unsafe { dl_iterate_phdr(Some(iterate_cb), state_ptr) };

        state.result
    }

    struct CallbackState {
        result: Result<Vec<SharedObject>, anyhow::Error>,
    }

    impl CallbackState {
        fn is_first(&self) -> bool {
            match &self.result {
                Ok(v) => v.is_empty(),
                Err(_) => false,
            }
        }
    }

    const CB_RESULT_OK: c_int = 0;
    const CB_RESULT_ERROR: c_int = -1;

    unsafe extern "C" fn iterate_cb(
        info: *mut dl_phdr_info,
        _size: size_t,
        data: *mut c_void,
    ) -> c_int {
        let state: *mut CallbackState = data.cast();

        // SAFETY: `data` is a pointer to a `CallbackState`, and no mutable reference
        // aliases with it in Rust. Furthermore, `dl_iterate_phdr` doesn't do anything
        // with `data` other than pass it to this callback, so nothing will be mutating
        // the object it points to while we're inside here.
        assert_pointer_valid(state);
        let state = unsafe { state.as_mut() }.expect("pointer is valid");

        // SAFETY: similarly, `dl_iterate_phdr` isn't mutating `info`
        // while we're here.
        assert_pointer_valid(info);
        let info = unsafe { info.as_ref() }.expect("pointer is valid");

        let base_address = usize::cast_from(info.dlpi_addr);

        let path_name = if state.is_first() {
            // From `man dl_iterate_phdr`:
            // "The first object visited by callback is the main program.  For the main
            // program, the dlpi_name field will be an empty string."
            match current_exe().context("failed to read the name of the current executable") {
                Ok(pb) => pb,
                Err(e) => {
                    // Profiles will be of dubious usefulness
                    // if we can't get the build ID for the main executable,
                    // so just bail here.
                    state.result = Err(e);
                    return CB_RESULT_ERROR;
                }
            }
        } else if info.dlpi_name.is_null() {
            // This would be unexpected, but let's handle this case gracefully by skipping this object.
            return CB_RESULT_OK;
        } else {
            // SAFETY: `dl_iterate_phdr` documents this as being a null-terminated string.
            assert_pointer_valid(info.dlpi_name);
            let name = unsafe { CStr::from_ptr(info.dlpi_name) };

            OsStr::from_bytes(name.to_bytes()).into()
        };

        // Walk the headers of this image, looking for `PT_LOAD` and `PT_NOTE` segments.
        let mut loaded_segments = Vec::new();
        let mut build_id = None;

        // SAFETY: `dl_iterate_phdr` is documented as setting `dlpi_phnum` to the
        // length of the array pointed to by `dlpi_phdr`.
        assert_pointer_valid(info.dlpi_phdr);
        let program_headers =
            unsafe { std::slice::from_raw_parts(info.dlpi_phdr, info.dlpi_phnum.into()) };

        for ph in program_headers {
            if ph.p_type == PT_LOAD {
                loaded_segments.push(LoadedSegment {
                    file_offset: u64::cast_from(ph.p_offset),
                    memory_offset: usize::cast_from(ph.p_vaddr),
                    memory_size: usize::cast_from(ph.p_memsz),
                });
            } else if ph.p_type == PT_NOTE {
                // From `man elf`:
                // typedef struct {
                //   Elf64_Word n_namesz;
                //   Elf64_Word n_descsz;
                //   Elf64_Word n_type;
                // } Elf64_Nhdr;
                #[repr(C)]
                struct NoteHeader {
                    n_namesz: Elf64_Word,
                    n_descsz: Elf64_Word,
                    n_type: Elf64_Word,
                }
                // This is how `man dl_iterate_phdr` says to find the
                // segment headers in memory.
                //
                // Note - it seems on some old
                // versions of Linux (I observed it on CentOS 7),
                // `p_vaddr` can be negative, so we use wrapping add here
                let mut offset = usize::cast_from(ph.p_vaddr.wrapping_add(info.dlpi_addr));
                let orig_offset = offset;

                const NT_GNU_BUILD_ID: Elf64_Word = 3;
                const GNU_NOTE_NAME: &[u8; 4] = b"GNU\0";
                const ELF_NOTE_STRING_ALIGN: usize = 4;

                while offset + std::mem::size_of::<NoteHeader>() + GNU_NOTE_NAME.len()
                    <= orig_offset + usize::cast_from(ph.p_memsz)
                {
                    // Justification: Our logic for walking this header
                    // follows exactly the code snippet in the
                    // `Notes (Nhdr)` section of `man elf`,
                    // so `offset` will always point to a `NoteHeader`
                    // (called `Elf64_Nhdr` in that document)
                    #[allow(clippy::as_conversions)]
                    let nh_ptr = offset as *const NoteHeader;

                    // SAFETY: Iterating according to the `Notes (Nhdr)`
                    // section of `man elf` ensures that this pointer is
                    // aligned. The offset check above ensures that it
                    // is in-bounds.
                    assert_pointer_valid(nh_ptr);
                    let nh = unsafe { nh_ptr.as_ref() }.expect("pointer is valid");

                    // from elf.h
                    if nh.n_type == NT_GNU_BUILD_ID
                        && nh.n_descsz != 0
                        && usize::cast_from(nh.n_namesz) == GNU_NOTE_NAME.len()
                    {
                        // Justification: since `n_namesz` is 4, the name is a four-byte value.
                        #[allow(clippy::as_conversions)]
                        let p_name = (offset + std::mem::size_of::<NoteHeader>()) as *const [u8; 4];

                        // SAFETY: since `n_namesz` is 4, the name is a four-byte value.
                        assert_pointer_valid(p_name);
                        let name = unsafe { p_name.as_ref() }.expect("pointer is valid");

                        if name == GNU_NOTE_NAME {
                            // We found what we're looking for!
                            // Justification: simple pointer arithmetic
                            #[allow(clippy::as_conversions)]
                            let p_desc = (p_name as usize + 4) as *const u8;

                            // SAFETY: This is the documented meaning of `n_descsz`.
                            assert_pointer_valid(p_desc);
                            let desc = unsafe {
                                std::slice::from_raw_parts(p_desc, usize::cast_from(nh.n_descsz))
                            };

                            build_id = Some(BuildId(desc.to_vec()));
                            break;
                        }
                    }
                    offset = offset
                        + std::mem::size_of::<NoteHeader>()
                        + align_up::<ELF_NOTE_STRING_ALIGN>(usize::cast_from(nh.n_namesz))
                        + align_up::<ELF_NOTE_STRING_ALIGN>(usize::cast_from(nh.n_descsz));
                }
            }
        }

        let objects = state.result.as_mut().expect("we return early on errors");
        objects.push(SharedObject {
            base_address,
            path_name,
            build_id,
            loaded_segments,
        });

        CB_RESULT_OK
    }

    /// Increases `p` as little as possible (including possibly 0)
    /// such that it becomes a multiple of `N`.
    pub const fn align_up<const N: usize>(p: usize) -> usize {
        if p % N == 0 {
            p
        } else {
            p + (N - (p % N))
        }
    }

    /// Asserts that the given pointer is valid.
    ///
    /// # Panics
    ///
    /// Panics if the given pointer:
    ///  * is a null pointer
    ///  * is not properly aligned for `T`
    fn assert_pointer_valid<T>(ptr: *const T) {
        // No other known way to convert a pointer to `usize`.
        #[allow(clippy::as_conversions)]
        let address = ptr as usize;
        let align = std::mem::align_of::<T>();

        assert!(!ptr.is_null());
        assert!(address % align == 0, "unaligned pointer");
    }

    fn current_exe_from_dladdr() -> Result<PathBuf, anyhow::Error> {
        let progname = unsafe {
            let mut dlinfo = std::mem::MaybeUninit::uninit();

            // This should set the filepath of the current executable
            // because it must contain the function pointer of itself.
            let ret = libc::dladdr(
                current_exe_from_dladdr as *const libc::c_void,
                dlinfo.as_mut_ptr(),
            );
            if ret == 0 {
                anyhow::bail!("dladdr failed");
            }
            CStr::from_ptr(dlinfo.assume_init().dli_fname).to_str()?
        };

        Ok(PathBuf::from_str(progname)?)
    }

    /// Get the name of the current executable by dladdr and fall back to std::env::current_exe
    /// if it fails. Try dladdr first because it returns the actual exe even when it's invoked
    /// by ld.so.
    fn current_exe() -> Result<PathBuf, anyhow::Error> {
        match current_exe_from_dladdr() {
            Ok(path) => Ok(path),
            Err(e) => {
                // when failed to get current exe from dladdr, fall back to the conventional way
                std::env::current_exe().context(e)
            }
        }
    }
}

#[cfg(target_os = "macos")]
use macos::collect_shared_objects;

#[cfg(target_os = "macos")]
mod macos {
    #![allow(non_camel_case_types, non_snake_case)]

    use std::{
        ffi::{CStr, OsStr},
        os::unix::ffi::OsStrExt,
        ptr, slice,
    };

    use mach2::{
        kern_return::KERN_SUCCESS,
        message::mach_msg_type_number_t,
        task::task_info,
        task_info::{task_dyld_info, TASK_DYLD_INFO},
        traps::mach_task_self,
        vm_prot::vm_prot_t,
        vm_types::natural_t,
    };
    use tracing::warn;

    use crate::{LoadedSegment, SharedObject};
    use util::{BuildId, CastFrom};

    #[repr(C, packed(4))]
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct dyld_image_info64 {
        pub load_address: u64,
        pub file_path: u64,
        pub file_mod_date: u64,
    }

    #[repr(C, packed(4))]
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct dyld_all_image_infos64 {
        pub version: u32,
        pub infoArrayCount: u32,
        pub infoArray: u64, // pointer to dyld_image_info
        pub notification: u64,
        pub processDetachedFromSharedRegion: bool,
    }

    #[repr(C, packed(4))]
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct mach_header64 {
        pub magic: u32,
        pub cputype: u32,
        pub cpusubtype: u32,
        pub filetype: u32,
        pub ncmds: u32,
        pub sizeofcmds: u32,
        pub flags: u32,
        pub reserved: u32,
    }

    #[repr(C, packed(4))]
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct load_command {
        pub cmd: u32,
        pub cmdsize: u32,
    }

    const LC_SEGMENT_64: u32 = 0x19;
    const LC_UUID: u32 = 0x1b;

    #[repr(C, packed(4))]
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct segment_command64 {
        pub cmd: u32,
        pub cmdsize: u32,
        pub segname: [u8; 16],
        pub vmaddr: u64,
        pub vmsize: u64,
        pub fileoff: u64,
        pub filesize: u64,
        pub maxprot: vm_prot_t,
        pub initprot: vm_prot_t,
        pub nsects: u32,
        pub flags: u32,
    }

    unsafe fn get_image_infos() -> Result<Vec<dyld_image_info64>, anyhow::Error> {
        let mut dyld_info: task_dyld_info = Default::default();
        let mut count: mach_msg_type_number_t =
            u32::try_from(size_of::<task_dyld_info>() / size_of::<natural_t>()).unwrap();

        let task = mach_task_self();
        let ret = task_info(
            task,
            TASK_DYLD_INFO,
            &mut dyld_info as *mut _ as *mut _,
            &mut count,
        );
        if ret != KERN_SUCCESS {
            anyhow::bail!("task_info returned {ret}");
        }

        let size = dyld_info.all_image_info_size;
        if size < u64::cast_from(size_of::<dyld_all_image_infos64>()) {
            anyhow::bail!("all_image_infos is too small (size is {}B)", size);
        }

        let all_image_infos = ptr::read_unaligned::<dyld_all_image_infos64>(
            dyld_info.all_image_info_addr as *const _,
        );

        let count = usize::cast_from(all_image_infos.infoArrayCount);

        let mut infos = Vec::with_capacity(count);
        slice::from_raw_parts_mut(
            infos.as_mut_ptr() as *mut u8,
            count * size_of::<dyld_image_info64>(),
        )
        .copy_from_slice(slice::from_raw_parts(
            all_image_infos.infoArray as *const u8,
            count * size_of::<dyld_image_info64>(),
        ));
        infos.set_len(count);

        Ok(infos)
    }

    unsafe fn get_shared_object(image: &dyld_image_info64) -> Result<SharedObject, anyhow::Error> {
        let header = ptr::read_unaligned::<mach_header64>(image.load_address as *const _);
        let path_name = if image.file_path != 0 {
            OsStr::from_bytes(CStr::from_ptr(image.file_path as *const i8).to_bytes())
        } else {
            OsStr::from_bytes(b"")
        };

        let mut cmd_address = usize::cast_from(image.load_address) + size_of::<mach_header64>();
        let mut next_cmd_address = cmd_address;
        let end_of_cmds = usize::cast_from(image.load_address)
            + size_of::<mach_header64>()
            + usize::cast_from(header.sizeofcmds);
        let mut loaded_segments = Vec::new();
        let mut build_id = None;

        for _ in 0..header.ncmds {
            let Some(lc) = (cmd_address + size_of::<load_command>() <= end_of_cmds)
                .then(|| ptr::read::<load_command>(cmd_address as *const load_command))
                .filter(|lc| {
                    next_cmd_address += usize::cast_from(lc.cmdsize);
                    next_cmd_address <= end_of_cmds
                })
            else {
                warn!(
                    "overrun parsing headers for {}",
                    path_name.to_string_lossy()
                );
                break;
            };

            if lc.cmd == LC_SEGMENT_64 {
                let seg_cmd = ptr::read_unaligned(cmd_address as *const segment_command64);

                loaded_segments.push(LoadedSegment {
                    file_offset: seg_cmd.fileoff,
                    memory_offset: usize::cast_from(seg_cmd.vmaddr),
                    memory_size: usize::cast_from(seg_cmd.vmsize),
                });
            } else if lc.cmd == LC_UUID {
                build_id = Some(BuildId(
                    slice::from_raw_parts(
                        (cmd_address + size_of::<load_command>()) as *mut u8,
                        usize::cast_from(lc.cmdsize) - size_of::<load_command>(),
                    )
                    .to_vec(),
                ));
            }

            cmd_address = next_cmd_address;
        }

        Ok(SharedObject {
            base_address: usize::cast_from(image.load_address),
            path_name: path_name.into(),
            build_id,
            loaded_segments,
        })
    }

    pub unsafe fn collect_shared_objects() -> Result<Vec<SharedObject>, anyhow::Error> {
        let mut objects = Vec::new();
        for info in get_image_infos()? {
            objects.push(get_shared_object(&info)?);
        }
        Ok(objects)
    }
}

/// Mappings of the processes' executable and shared libraries.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub static MAPPINGS: Lazy<Option<Vec<Mapping>>> = Lazy::new(|| {
    /// Build a list of mappings for the passed shared objects.
    fn build_mappings(objects: &[SharedObject]) -> Vec<Mapping> {
        let mut mappings = Vec::new();
        for object in objects {
            for segment in &object.loaded_segments {
                // I have observed that `memory_offset` can be negative on some very old
                // versions of Linux (e.g. CentOS 7), so use wrapping add here.
                let memory_start = object.base_address.wrapping_add(segment.memory_offset);
                mappings.push(Mapping {
                    memory_start,
                    memory_end: memory_start + segment.memory_size,
                    memory_offset: segment.memory_offset,
                    file_offset: segment.file_offset,
                    pathname: object.path_name.clone(),
                    build_id: object.build_id.clone(),
                });
            }
        }
        mappings
    }

    // SAFETY: We are on Linux or MacOS
    match unsafe { collect_shared_objects() } {
        Ok(objects) => Some(build_mappings(&objects)),
        Err(err) => {
            error!("build ID fetching failed: {err}");
            None
        }
    }
});

#[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
pub static MAPPINGS: Lazy<Option<Vec<Mapping>>> = Lazy::new(|| {
    error!("build ID fetching is only supported on Linux or MacOS");
    None
});

/// Information about a shared object loaded into the current process.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SharedObject {
    /// The address at which the object is loaded.
    pub base_address: usize,
    /// The path of that file the object was loaded from.
    pub path_name: PathBuf,
    /// The build ID of the object, if found.
    pub build_id: Option<BuildId>,
    /// Loaded segments of the object.
    pub loaded_segments: Vec<LoadedSegment>,
}

/// A segment of a shared object that's loaded into memory.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LoadedSegment {
    /// Offset of the segment in the source file.
    pub file_offset: u64,
    /// Offset to the `SharedObject`'s `base_address`.
    pub memory_offset: usize,
    /// Size of the segment in memory.
    pub memory_size: usize,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn test_collect_shared_objects() {
    let objects = unsafe { collect_shared_objects() }.unwrap();
    println!("{:#?}", objects);
}
