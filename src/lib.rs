//! twig: instant, nearly free git branches to work on in parallel.
//!
//! `twig new fix-login` makes a copy-on-write clone of the repository (APFS `clonefile`,
//! btrfs/xfs reflinks), build output included, on its own branch: it takes seconds and
//! almost no disk, and it builds warm. Work there with any tool, then `twig land` puts the
//! changes back into the main checkout with a three-way merge.

pub mod clone;
pub mod git;
pub mod store;

pub use store::{Created, Repo, Report, Twig};
