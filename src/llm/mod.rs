mod embedded;
mod split;

pub use embedded::{generate, init_backend, GenerateConfig};
pub use split::{split_lower, split_upper, SplitLowerConfig, SplitLowerOutput, SplitUpperConfig};
