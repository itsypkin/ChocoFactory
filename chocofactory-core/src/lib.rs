//! Shared types used by `chocofactoryd` and `choco`: `Event`, DB models,
//! workflow definition types. Populated incrementally by later tickets
//! (see .agents/ChocoFactory/04-plan.md).

pub mod daemon_lock;
pub mod mcp;
pub mod models;
pub mod paths;
pub mod version;
