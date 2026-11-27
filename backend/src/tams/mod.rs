//! TAMS (Time-Addressable Media Store) integration.
//!
//! Talks to an Eyevinn TAMS Gateway (CouchDB index + S3 essence, presigned URLs).
//! See `docs/archive/TAMS_INTEGRATION_PLAN.md` for the design and phasing.

pub mod auth;
pub mod client;
pub mod flow_claims;
pub mod park;
pub mod segmenter;
pub mod tai_mapping;
pub mod uploader;

pub use client::{FlowSpec, TamsClient, FORMAT_AUDIO, FORMAT_VIDEO};
pub use uploader::{channel, spawn_uploader, FragmentReady};
