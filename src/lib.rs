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

#![doc = include_str!("../README.md")]

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use std::ffi::CString;
use std::io::BufRead;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

use anyhow::bail;
use libc::size_t;
use once_cell::sync::Lazy;
use tempfile::NamedTempFile;
use tikv_jemalloc_ctl::raw;
use tokio::sync::Mutex;
use tracing::error;
use flate2::write::GzEncoder;
use flate2::Compression;
use prost::Message;

mod linux;
use linux::BuildId;

mod cast;
use cast::{CastFrom, TryCastFrom};

#[path = "perftools.profiles.rs"]
mod pprof_types;

/// Start times of the profiler.
#[derive(Copy, Clone, Debug)]
pub enum ProfStartTime {
    Instant(Instant),
    TimeImmemorial,
}

/// A single sample in the profile. The stack is a list of addresses.
#[derive(Clone, Debug)]
struct WeightedStack {
    pub addrs: Vec<usize>,
    pub weight: f64,
}

/// A mapping of a single shared object.
#[derive(Clone, Debug)]
struct Mapping {
    pub memory_start: usize,
    pub memory_end: usize,
    pub memory_offset: usize,
    pub file_offset: u64,
    pub pathname: PathBuf,
    pub build_id: Option<BuildId>,
}

/// A minimal representation of a profile that can be parsed from the jemalloc heap profile.
#[derive(Default)]
pub struct StackProfile {
    annotations: Vec<String>,
    // The second element is the index in `annotations`, if one exists.
    stacks: Vec<(WeightedStack, Option<usize>)>,
    mappings: Vec<Mapping>,
}

impl StackProfile {
    /// Converts the profile into the pprof format.
    ///
    /// pprof encodes profiles as gzipped protobuf messages of the Profile message type
    /// (see `pprof/profile.proto`).
    pub fn to_pprof(
        &self,
        sample_type: (&str, &str),
        period_type: (&str, &str),
        anno_key: Option<String>,
    ) -> Vec<u8> {
        use crate::pprof_types as proto;

        let mut profile = proto::Profile::default();
        let mut strings = StringTable::new();

        let anno_key = anno_key.unwrap_or_else(|| "annotation".into());

        profile.sample_type = vec![proto::ValueType {
            r#type: strings.insert(sample_type.0),
            unit: strings.insert(sample_type.1),
        }];
        profile.period_type = Some(proto::ValueType {
            r#type: strings.insert(period_type.0),
            unit: strings.insert(period_type.1),
        });

        profile.time_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("now is later than UNIX epoch")
            .as_nanos()
            .try_into()
            .expect("the year 2554 is far away");

        for (mapping, mapping_id) in self.mappings.iter().zip(1..) {
            let pathname = mapping.pathname.to_string_lossy();
            let filename_idx = strings.insert(&pathname);

            let build_id_idx = match &mapping.build_id {
                Some(build_id) => strings.insert(&build_id.to_string()),
                None => 0,
            };

            profile.mapping.push(proto::Mapping {
                id: mapping_id,
                memory_start: u64::cast_from(mapping.memory_start),
                memory_limit: u64::cast_from(mapping.memory_end),
                file_offset: mapping.file_offset,
                filename: filename_idx,
                build_id: build_id_idx,
                ..Default::default()
            });

            // This is a is a Polar Signals-specific extension: For correct offline symbolization
            // they need access to the memory offset of mappings, but the pprof format only has a
            // field for the file offset. So we instead encode additional information about
            // mappings in magic comments. There must be exactly one comment for each mapping.

            // Take a shortcut and assume the ELF type is always `ET_DYN`. This is true for shared
            // libraries and for position-independent executable, so it should always be true for
            // any mappings we have.
            // Getting the actual information is annoying. It's in the ELF header (the `e_type`
            // field), but there is no guarantee that the full ELF header gets mapped, so we might
            // not be able to find it in memory. We could try to load it from disk instead, but
            // then we'd have to worry about blocking disk I/O.
            let elf_type = 3;

            let comment = format!(
                "executableInfo={:x};{:x};{:x}",
                elf_type, mapping.file_offset, mapping.memory_offset
            );
            profile.comment.push(strings.insert(&comment));
        }

        let mut location_ids = BTreeMap::new();
        for (stack, anno) in self.iter() {
            let mut sample = proto::Sample::default();

            let value = stack.weight.trunc();
            let value = i64::try_cast_from(value).expect("no exabyte heap sizes");
            sample.value.push(value);

            for addr in stack.addrs.iter().rev() {
                // See the comment
                // [here](https://github.com/rust-lang/backtrace-rs/blob/036d4909e1fb9c08c2bb0f59ac81994e39489b2f/src/symbolize/mod.rs#L123-L147)
                // for why we need to subtract one. tl;dr addresses
                // in stack traces are actually the return address of
                // the called function, which is one past the call
                // itself.
                //
                // Of course, the `call` instruction can be more than one byte, so after subtracting
                // one, we might point somewhere in the middle of it, rather
                // than to the beginning of the instruction. That's fine; symbolization
                // tools don't seem to get confused by this.
                let addr = u64::cast_from(*addr) - 1;

                let loc_id = *location_ids.entry(addr).or_insert_with(|| {
                    // pprof_types.proto says the location id may be the address, but Polar Signals
                    // insists that location ids are sequential, starting with 1.
                    let id = u64::cast_from(profile.location.len()) + 1;
                    let mapping_id = profile
                        .mapping
                        .iter()
                        .find(|m| m.memory_start <= addr && m.memory_limit > addr)
                        .map_or(0, |m| m.id);
                    profile.location.push(proto::Location {
                        id,
                        mapping_id,
                        address: addr,
                        ..Default::default()
                    });
                    id
                });

                sample.location_id.push(loc_id);

                if let Some(anno) = anno {
                    sample.label.push(proto::Label {
                        key: strings.insert(&anno_key),
                        str: strings.insert(anno),
                        ..Default::default()
                    })
                }
            }

            profile.sample.push(sample);
        }

        profile.string_table = strings.finish();

        let encoded = profile.encode_to_vec();

        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&encoded).unwrap();
        gz.finish().unwrap()
    }
}

/// Helper struct to simplify building a `string_table` for the pprof format.
#[derive(Default)]
struct StringTable(BTreeMap<String, i64>);

impl StringTable {
    fn new() -> Self {
        // Element 0 must always be the emtpy string.
        let inner = [("".into(), 0)].into();
        Self(inner)
    }

    fn insert(&mut self, s: &str) -> i64 {
        if let Some(idx) = self.0.get(s) {
            *idx
        } else {
            let idx = i64::try_from(self.0.len()).expect("must fit");
            self.0.insert(s.into(), idx);
            idx
        }
    }

    fn finish(self) -> Vec<String> {
        let mut vec: Vec<_> = self.0.into_iter().collect();
        vec.sort_by_key(|(_, idx)| *idx);
        vec.into_iter().map(|(s, _)| s).collect()
    }
}

struct StackProfileIter<'a> {
    inner: &'a StackProfile,
    idx: usize,
}

impl<'a> Iterator for StackProfileIter<'a> {
    type Item = (&'a WeightedStack, Option<&'a str>);

    fn next(&mut self) -> Option<Self::Item> {
        let (stack, anno) = self.inner.stacks.get(self.idx)?;
        self.idx += 1;
        let anno = anno.map(|idx| self.inner.annotations.get(idx).unwrap().as_str());
        Some((stack, anno))
    }
}

impl StackProfile {
    fn push_stack(&mut self, stack: WeightedStack, annotation: Option<&str>) {
        let anno_idx = if let Some(annotation) = annotation {
            Some(
                self.annotations
                    .iter()
                    .position(|anno| annotation == anno.as_str())
                    .unwrap_or_else(|| {
                        self.annotations.push(annotation.to_string());
                        self.annotations.len() - 1
                    }),
            )
        } else {
            None
        };
        self.stacks.push((stack, anno_idx))
    }

    fn push_mapping(&mut self, mapping: Mapping) {
        self.mappings.push(mapping);
    }

    fn iter(&self) -> StackProfileIter<'_> {
        StackProfileIter {
            inner: self,
            idx: 0,
        }
    }
}

/// Activate jemalloc profiling.
pub async fn activate_jemalloc_profiling() {
    let Some(ctl) = PROF_CTL.as_ref() else {
        tracing::warn!("jemalloc profiling is disabled and cannot be activated");
        return;
    };

    let mut ctl = ctl.lock().await;
    if ctl.activated() {
        return;
    }

    match ctl.activate() {
        Ok(()) => tracing::info!("jemalloc profiling activated"),
        Err(err) => tracing::warn!("could not activate jemalloc profiling: {err}"),
    }
}

/// Deactivate jemalloc profiling.
pub async fn deactivate_jemalloc_profiling() {
    let Some(ctl) = PROF_CTL.as_ref() else {
        return; // jemalloc not enabled
    };

    let mut ctl = ctl.lock().await;
    if !ctl.activated() {
        return;
    }

    match ctl.deactivate() {
        Ok(()) => tracing::info!("jemalloc profiling deactivated"),
        Err(err) => tracing::warn!("could not deactivate jemalloc profiling: {err}"),
    }
}

/// Per-process singleton for controlling jemalloc profiling.
pub static PROF_CTL: Lazy<Option<Arc<Mutex<JemallocProfCtl>>>> = Lazy::new(|| {
    if let Some(ctl) = JemallocProfCtl::get() {
        Some(Arc::new(Mutex::new(ctl)))
    } else {
        None
    }
});

/// Mappings of the processes' executable and shared libraries.
#[cfg(target_os = "linux")]
static MAPPINGS: Lazy<Option<Vec<Mapping>>> = Lazy::new(|| {
    use crate::linux::SharedObject;

    /// Build a list of mappings for the passed shared objects.
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

/// Metadata about a jemalloc heap profiler.
#[derive(Copy, Clone, Debug)]
pub struct JemallocProfMetadata {
    pub start_time: Option<ProfStartTime>,
}

/// A handle to control jemalloc profiling.
#[derive(Debug)]
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

    /// Returns the metadata of the profiler.
    pub fn get_md(&self) -> JemallocProfMetadata {
        self.md
    }

    /// Returns whether the profiler is active.
    pub fn activated(&self) -> bool {
        self.md.start_time.is_some()
    }

    /// Activate the profiler and if unset, set the start time to the current time.
    pub fn activate(&mut self) -> Result<(), tikv_jemalloc_ctl::Error> {
        // SAFETY: "prof.active" is documented as being writable and taking a bool:
        // http://jemalloc.net/jemalloc.3.html#prof.active
        unsafe { raw::write(b"prof.active\0", true) }?;
        if self.md.start_time.is_none() {
            self.md.start_time = Some(ProfStartTime::Instant(Instant::now()));
        }
        Ok(())
    }

    /// Deactivate the profiler.
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

    /// Dump a profile into a temporary file and return it.
    pub fn dump(&mut self) -> anyhow::Result<std::fs::File> {
        let f = NamedTempFile::new()?;
        let path = CString::new(f.path().as_os_str().as_bytes().to_vec()).unwrap();

        // SAFETY: "prof.dump" is documented as being writable and taking a C string as input:
        // http://jemalloc.net/jemalloc.3.html#prof.dump
        unsafe { raw::write(b"prof.dump\0", path.as_ptr()) }?;
        Ok(f.into_file())
    }
}
