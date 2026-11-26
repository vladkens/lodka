// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! lodka's VMM on Hypervisor.framework: a machine with its devices that boots an arm64 Linux
//! kernel. It knows nothing of lodka's commands, files or settings; the `lodka` binary builds
//! the VM from them.

mod fdt;
mod hv;
pub mod machine;
mod mem;
