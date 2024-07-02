use std::time::Instant;

pub use pure::{parse_jeheap, Mapping, StackProfile, StackProfileIter, WeightedStack, MAPPINGS};

/// Start times of the profiler.
#[derive(Copy, Clone, Debug)]
pub enum ProfStartTime {
    Instant(Instant),
    TimeImmemorial,
}
