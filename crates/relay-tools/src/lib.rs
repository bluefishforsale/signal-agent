//! Every MCP tool, and the registry that decides which ones a caller may see
//! and run. Access rules live in the registry, not in each tool, so a new tool
//! can't forget them.

mod directory;
mod input;
mod limits;
mod local;
mod lookup;
mod media;
mod mutate;
mod read;
mod registry;
mod send;
mod settings;
#[cfg(test)]
mod testing;
mod tool;
mod webhook;

pub use limits::{Clock, LimitExceeded, Limiter, Limits, SystemClock, begin_provisioning};
pub use registry::Registry;
pub use settings::Settings;
pub use tool::{Tool, ToolContext, ToolError};
pub use webhook::WebhookUrl;

pub mod prelude {
    pub use crate::{Registry, Tool, ToolContext, ToolError};
}
