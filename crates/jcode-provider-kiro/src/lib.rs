//! Pure (I/O-free) building blocks for the Kiro provider.
//!
//! Kiro talks to the AWS CodeWhisperer streaming API
//! (`GenerateAssistantResponse`). Requests are JSON, responses are framed with
//! the AWS event stream binary encoding (`application/vnd.amazon.eventstream`).
//! This crate owns the parts of that contract that can be expressed without
//! networking so they stay small, reviewable, and reusable:
//!
//! - [`eventstream`]: decoder for the binary event stream framing.
//! - [`models`]: the known Kiro model catalog and model-id normalization.
//! - [`request`]: jcode messages/tools -> `GenerateAssistantResponse` body.
//! - [`stream`]: decoded stream events -> jcode [`StreamEvent`]s.
//! - [`errors`]: HTTP failure classification and messages.
//!
//! [`StreamEvent`]: jcode_message_types::StreamEvent

pub mod errors;
pub mod eventstream;
pub mod models;
pub mod request;
pub mod stream;

pub use models::{DEFAULT_MODEL, KIRO_MODELS, KiroModel};
