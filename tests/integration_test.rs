//! Integration tests — 12 end-to-end scenarios over real HTTP.

#[path = "integration/common.rs"]
mod common;

#[path = "integration/authz.rs"]
mod authz;
#[path = "integration/chunking.rs"]
mod chunking;
#[path = "integration/conditional.rs"]
mod conditional;
#[path = "integration/dedup.rs"]
mod dedup;
#[path = "integration/deletion.rs"]
mod deletion;
#[path = "integration/encryption.rs"]
mod encryption;
#[path = "integration/health.rs"]
mod health;
#[path = "integration/multipart.rs"]
mod multipart;
#[path = "integration/perf.rs"]
mod perf;
#[path = "integration/range.rs"]
mod range;
#[path = "integration/ratelimit.rs"]
mod ratelimit;
#[path = "integration/replication.rs"]
mod replication;
#[path = "integration/roundtrip.rs"]
mod roundtrip;
