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

//! Various profiling utilities:
//!
//! (1) Turn jemalloc profiling on and off, and dump heap profiles (`PROF_CTL`)
//! (2) Parse jemalloc heap files and make them into a hierarchical format (`parse_jeheap`)

use std::ffi::CString;
use std::io::BufRead;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;
use std::time::Instant;

use anyhow::bail;
use libc::size_t;
use once_cell::sync::Lazy;
use tempfile::NamedTempFile;
use tikv_jemalloc_ctl::raw;
use tokio::sync::Mutex;
use tracing::error;

use crate::{Mapping, ProfStartTime, StackProfile, WeightedStack};

pub static PROF_CTL: Lazy<Option<Arc<Mutex<JemallocProfCtl>>>> = Lazy::new(|| {
    if let Some(ctl) = JemallocProfCtl::get() {
        Some(Arc::new(Mutex::new(ctl)))
    } else {
        None
    }
});

#[cfg(target_os = "linux")]
static MAPPINGS: Lazy<Option<Vec<Mapping>>> = Lazy::new(|| {
    use crate::linux::SharedObject;

    fn build_mappings(objects: &[SharedObject]) -> Vec<Mapping> {
        let mut mappings = Vec::new();
        for object in objects {
            for segment in &object.loaded_segments {
                let memory_start = object.base_address + segment.memory_offset;
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

    // SAFETY: We are on Linux, and this is the only place in the program this
    // function is called.
    match unsafe { crate::linux::collect_shared_objects() } {
        Ok(objects) => Some(build_mappings(&objects)),
        Err(err) => {
            error!("build ID fetching failed: {err}");
            None
        }
    }
});

#[cfg(not(target_os = "linux"))]
static MAPPINGS: Lazy<Option<Vec<Mapping>>> = Lazy::new(|| {
    error!("build ID fetching is only supported on Linux");
    None
});

#[derive(Copy, Clone, Debug)]
pub struct JemallocProfMetadata {
    pub start_time: Option<ProfStartTime>,
}

#[derive(Debug)]
// Per-process singleton object allowing control of jemalloc profiling facilities.
pub struct JemallocProfCtl {
    md: JemallocProfMetadata,
}

/// Parse a jemalloc profile file, producing a vector of stack traces along with their weights.
pub fn parse_jeheap<R: BufRead>(r: R) -> anyhow::Result<StackProfile> {
    let mut cur_stack = None;
    let mut profile = StackProfile::default();
    let mut lines = r.lines();

    let first_line = match lines.next() {
        Some(s) => s?,
        None => bail!("Heap dump file was empty"),
    };
    // The first line of the file should be e.g. "heap_v2/524288", where the trailing
    // number is the inverse probability of a byte being sampled.
    let sampling_rate: f64 = str::parse(first_line.trim_start_matches("heap_v2/"))?;

    while let Some(line) = lines.next() {
        let line = line?;
        let line = line.trim();

        let words: Vec<_> = line.split_ascii_whitespace().collect();
        if words.len() > 0 && words[0] == "@" {
            if cur_stack.is_some() {
                bail!("Stack without corresponding weight!")
            }
            let mut addrs = words[1..]
                .iter()
                .map(|w| {
                    let raw = w.trim_start_matches("0x");
                    usize::from_str_radix(raw, 16)
                })
                .collect::<Result<Vec<_>, _>>()?;
            addrs.reverse();
            cur_stack = Some(addrs);
        }
        if words.len() > 2 && words[0] == "t*:" {
            if let Some(addrs) = cur_stack.take() {
                // The format here is e.g.:
                // t*: 40274: 2822125696 [0: 0]
                //
                // "t*" means summary across all threads; someday we will support per-thread dumps but don't now.
                // "40274" is the number of sampled allocations (`n_objs` here).
                // On all released versions of jemalloc, "2822125696" is the total number of bytes in those allocations.
                //
                // To get the predicted number of total bytes from the sample, we need to un-bias it by following the logic in
                // jeprof's `AdjustSamples`: https://github.com/jemalloc/jemalloc/blob/498f47e1ec83431426cdff256c23eceade41b4ef/bin/jeprof.in#L4064-L4074
                //
                // However, this algorithm is actually wrong: you actually need to unbias each sample _before_ you add them together, rather
                // than adding them together first and then unbiasing the average allocation size. But the heap profile format in released versions of jemalloc
                // does not give us access to each individual allocation, so this is the best we can do (and `jeprof` does the same).
                //
                // It usually seems to be at least close enough to being correct to be useful, but could be very wrong if for the same stack, there is a
                // very large amount of variance in the amount of bytes allocated (e.g., if there is one allocation of 8 MB and 1,000,000 of 8 bytes)
                //
                // In the latest unreleased jemalloc sources from github, the issue is worked around by unbiasing the numbers for each sampled allocation,
                // and then fudging them to maintain compatibility with jeprof's logic. So, once those are released and we start using them,
                // this will become even more correct.
                //
                // For more details, see this doc: https://github.com/jemalloc/jemalloc/pull/1902
                //
                // And this gitter conversation between me (Brennan Vincent) and David Goldblatt: https://gitter.im/jemalloc/jemalloc?at=5f31b673811d3571b3bb9b6b
                let n_objs: f64 = str::parse(words[1].trim_end_matches(':'))?;
                let bytes_in_sampled_objs: f64 = str::parse(words[2])?;
                let ratio = (bytes_in_sampled_objs / n_objs) / sampling_rate;
                let scale_factor = 1.0 / (1.0 - (-ratio).exp());
                let weight = bytes_in_sampled_objs * scale_factor;
                profile.push_stack(WeightedStack { addrs, weight }, None);
            }
        }
    }
    if cur_stack.is_some() {
        bail!("Stack without corresponding weight!");
    }

    if let Some(mappings) = MAPPINGS.as_ref() {
        for mapping in mappings {
            profile.push_mapping(mapping.clone());
        }
    }

    Ok(profile)
}

impl JemallocProfCtl {
    // Creates and returns the global singleton.
    fn get() -> Option<Self> {
        // SAFETY: "opt.prof" is documented as being readable and returning a bool:
        // http://jemalloc.net/jemalloc.3.html#opt.prof
        let prof_enabled: bool = unsafe { raw::read(b"opt.prof\0") }.unwrap();
        if prof_enabled {
            // SAFETY: "opt.prof_active" is documented as being readable and returning a bool:
            // http://jemalloc.net/jemalloc.3.html#opt.prof_active
            let prof_active: bool = unsafe { raw::read(b"opt.prof_active\0") }.unwrap();
            let start_time = if prof_active {
                Some(ProfStartTime::TimeImmemorial)
            } else {
                None
            };
            let md = JemallocProfMetadata { start_time };
            Some(Self { md })
        } else {
            None
        }
    }

    /// Returns the base 2 logarithm of the sample rate (average interval, in bytes, between allocation samples).
    pub fn lg_sample(&self) -> size_t {
        // SAFETY: "prof.lg_sample" is documented as being readable and returning size_t:
        // https://jemalloc.net/jemalloc.3.html#opt.lg_prof_sample
        unsafe { raw::read(b"prof.lg_sample\0") }.unwrap()
    }

    pub fn get_md(&self) -> JemallocProfMetadata {
        self.md
    }

    pub fn activated(&self) -> bool {
        self.md.start_time.is_some()
    }

    pub fn activate(&mut self) -> Result<(), tikv_jemalloc_ctl::Error> {
        // SAFETY: "prof.active" is documented as being writable and taking a bool:
        // http://jemalloc.net/jemalloc.3.html#prof.active
        unsafe { raw::write(b"prof.active\0", true) }?;
        if self.md.start_time.is_none() {
            self.md.start_time = Some(ProfStartTime::Instant(Instant::now()));
        }
        Ok(())
    }

    pub fn deactivate(&mut self) -> Result<(), tikv_jemalloc_ctl::Error> {
        // SAFETY: "prof.active" is documented as being writable and taking a bool:
        // http://jemalloc.net/jemalloc.3.html#prof.active
        unsafe { raw::write(b"prof.active\0", false) }?;
        let rate = self.lg_sample();
        // SAFETY: "prof.reset" is documented as being writable and taking a size_t:
        // http://jemalloc.net/jemalloc.3.html#prof.reset
        unsafe { raw::write(b"prof.reset\0", rate) }?;

        self.md.start_time = None;
        Ok(())
    }

    pub fn dump(&mut self) -> anyhow::Result<std::fs::File> {
        let f = NamedTempFile::new()?;
        let path = CString::new(f.path().as_os_str().as_bytes().to_vec()).unwrap();

        // SAFETY: "prof.dump" is documented as being writable and taking a C string as input:
        // http://jemalloc.net/jemalloc.3.html#prof.dump
        unsafe { raw::write(b"prof.dump\0", path.as_ptr()) }?;
        Ok(f.into_file())
    }
}
