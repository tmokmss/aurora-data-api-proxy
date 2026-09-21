//! A PostgreSQL wire-protocol front end for the Aurora RDS Data API.
//!
//! The proxy speaks the PostgreSQL frontend/backend protocol on a local socket
//! and translates each query into an `ExecuteStatement` HTTP call. That lets
//! ordinary PostgreSQL clients -- `psql`, GUI tools, and the drivers of any
//! language -- reach an Aurora cluster over IAM alone, with no bastion host and
//! no VPN.
//!
//! The interesting part is the gap between the two protocols. The Data API is
//! stateless: one SQL string in, one result out, with no connection, no
//! prepared statements and nothing resembling the protocol's `Describe`
//! message. Most clients, meanwhile, use the extended query protocol. Bridging
//! that is what [`handlers`] and [`probe`] are about.

// Errors here are pgwire's `ErrorInfo`, which is large because a PostgreSQL
// error message has a dozen optional fields. Boxing it would save a memcpy on a
// path that has just made an HTTP request, so the type is carried by value and
// handed to pgwire, which boxes it at the protocol boundary.
#![allow(clippy::result_large_err)]

pub mod config;
pub mod dataapi;
pub mod exec;
pub mod handlers;
pub mod params;
pub mod probe;
pub mod session;
pub mod sql;
pub mod types;
