//! rr process control, replay replicas and the live gdbserver target.
//!
//! - [`Rr`] finds and version-checks `rr` (5.9 and 5.10 are supported) and runs `rr record`,
//!   `rr ps`, `rr traceinfo` and `rr pack`. A recording's id is its trace uuid.
//! - [`RrReplica`] is one `rr replay -s` server as a full [`hopper_core::target::Target`]:
//!   reverse execution, seeks to any [`hopper_core::Moment`], checkpoints, and rr's clock plus
//!   hopper's own instruction counter `sub`.
//! - [`GdbserverTarget`] is a live process under gdbserver, forward only, with synthetic time and
//!   a random recording id; it is what tests use on machines without a PMU.
//! - [`ReplicaPool`] owns one cursor replica and N background replicas, each on its own worker
//!   thread, started lazily by a [`ReplicaFactory`].
//! - [`Auxv`] parses the auxiliary vector and gives the main executable's load bias.

pub mod auxv;
pub mod gdbserver;
pub mod pool;
mod remote;
pub mod replica;
pub mod rr;
pub mod server;

pub use auxv::Auxv;
pub use gdbserver::{GdbserverConfig, GdbserverTarget, OutputPolicy};
pub use pool::{
    GdbserverFactory, Lease, Pending, ReplicaFactory, ReplicaPool, ReplicaRole, RrReplicaFactory,
};
pub use replica::{ReplayOptions, RrReplica};
pub use rr::{
    PackMode, ProcessExit, PsRow, RecordOptions, Recorded, Rr, RrVersion, TraceInfo, parse_ps,
    parse_traceinfo,
};
