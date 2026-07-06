//! Declarative API tools — TOML-driven HTTP tool definitions.

pub mod provider;
pub mod loader;
pub mod register;

pub use provider::DeclarativeApiModule;
pub use register::ApiToolRegisterModule;
