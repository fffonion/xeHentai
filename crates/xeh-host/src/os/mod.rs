//! Host-owned operating-system primitives that the pinned Core does not
//! publish. Business rules stay in RSS; this module only resolves typed named
//! requests against a trusted per-VM policy and performs the requested effect.

pub mod fs;

pub use fs::{
    functions as fs_functions, install, register as register_fs, structs as fs_structs, FsState,
};
