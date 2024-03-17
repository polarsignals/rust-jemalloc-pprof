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

use std::ffi::CString;

use std::io::BufReader;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;
use std::time::Instant;

use once_cell::sync::Lazy;
use pure::parse_jeheap;
use libc::size_t;

use tempfile::NamedTempFile;
use tikv_jemalloc_ctl::raw;
use tokio::sync::Mutex;

/// Start times of the profiler.
#[derive(Copy, Clone, Debug)]
pub enum ProfStartTime {
    Instant(Instant),
    TimeImmemorial,
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

    /// Dump a profile in pprof format (gzipped protobuf) and
    /// return a buffer with its contents.
    pub fn dump_pprof(&mut self) -> anyhow::Result<Vec<u8>> {
        let f = self.dump()?;
        let dump_reader = BufReader::new(f);
        let profile = parse_jeheap(dump_reader)?;
        let pprof = profile.to_pprof(("inuse_space", "bytes"), ("space", "bytes"), None);
        Ok(pprof)
    }
}
