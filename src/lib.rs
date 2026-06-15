//! Library for generating SCIP code-intelligence indexes from shell scripts.
//!
//! The entry point is [`indexer::index_document`], which parses a single shell
//! script with `brush-parser` and emits a SCIP [`scip::types::Document`].

pub mod expansions;
pub mod indexer;
pub mod range;
pub mod resolve;
pub mod symbols;
