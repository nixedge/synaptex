/// Re-exports all generated types and service traits for `synaptex.v1`.
pub mod synaptex {
    pub mod v1 {
        tonic::include_proto!("synaptex.v1");
    }
}

// Flatten the module hierarchy for convenience.
pub use synaptex::v1::*;
