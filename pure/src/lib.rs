use anyhow::bail;
use cast::{CastFrom, TryCastFrom};
use flate2::write::GzEncoder;
use flate2::Compression;
use prost::Message;
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::linux::{Mapping, MAPPINGS};

mod cast;
mod linux;

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

#[path = "perftools.profiles.rs"]
mod pprof_types;

/// A single sample in the profile. The stack is a list of addresses.
#[derive(Clone, Debug)]
pub struct WeightedStack {
    pub addrs: Vec<usize>,
    pub weight: f64,
}

/// A minimal representation of a profile that can be parsed from the jemalloc heap profile.
#[derive(Default)]
pub struct StackProfile {
    pub annotations: Vec<String>,
    // The second element is the index in `annotations`, if one exists.
    pub stacks: Vec<(WeightedStack, Option<usize>)>,
    pub mappings: Vec<Mapping>,
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

pub struct StackProfileIter<'a> {
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
    pub fn push_stack(&mut self, stack: WeightedStack, annotation: Option<&str>) {
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

    pub fn push_mapping(&mut self, mapping: Mapping) {
        self.mappings.push(mapping);
    }

    pub fn iter(&self) -> StackProfileIter<'_> {
        StackProfileIter {
            inner: self,
            idx: 0,
        }
    }
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
