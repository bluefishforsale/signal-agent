//! Every MCP tool, and the registry that decides which ones a caller may see
//! and run. Access rules live in the registry, not in each tool, so a new tool
//! can't forget them.

mod registry;
mod tool;

pub use registry::Registry;
pub use tool::{Tool, ToolContext, ToolError};

pub mod prelude {
    pub use crate::{Registry, Tool, ToolContext, ToolError};
}
