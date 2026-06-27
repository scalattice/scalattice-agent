mod embedded;
mod split;

pub use embedded::{embedded_available, generate, init_backend, GenerateConfig, GenerateOutput};
pub use split::{split_lower, split_upper, SplitLowerConfig, SplitLowerOutput, SplitUpperConfig};
