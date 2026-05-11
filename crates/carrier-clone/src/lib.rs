//! Carrier Clone — .agx template extraction and manifest building.
//!
//! v3: "源码格式 = 运行时格式，解压即 workspace，无转换"。
//! The .agx IS the workspace, just compressed.

pub mod extractor;
pub mod hub;
pub mod manifest_builder;
mod loader;

pub use extractor::{extract_agx, pack_workspace_as_agx, scan_workspace_security};
pub use loader::{
    format_string_array, parse_frontmatter, parse_string_array, parse_toml_description,
    TemplateManifest,
};
pub use manifest_builder::build_manifest_from_workspace;
