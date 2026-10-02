#![doc = include_str!("../README.md")]
#![allow(clippy::needless_doctest_main)]

pub mod decider;
pub mod repository;
pub mod strategies;

#[cfg(test)]
mod test_helpers;
