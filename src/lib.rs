// tonic::Status sits in the Err position of every generated signature and is
// larger than clippy's threshold. Nothing here is hand-written, so the lint has
// nothing actionable to say.
#![allow(clippy::result_large_err)]

pub mod cache {
    tonic::include_proto!("cache");
}
