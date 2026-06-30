mod embedded;
mod model_cache;
mod prompt;
mod split;

pub use embedded::{generate, init_backend, GenerateConfig};
pub use model_cache::{evict_all, preload_model};
pub use split::{split_lower, split_upper, SplitLowerConfig, SplitLowerOutput, SplitUpperConfig};
