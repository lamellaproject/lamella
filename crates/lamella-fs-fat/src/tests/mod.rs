//! The crate's own tests. They live under `src/` rather than in `tests/` because they reach
//! crate-private items -- `crate::boot`, `crate::dir` and `crate::fat` are private modules, and an
//! integration test links this crate from outside and cannot see them. Gathering them in one
//! directory keeps `src/` listing the driver's own files.

mod end_to_end;
