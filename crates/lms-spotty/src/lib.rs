//! Unified `lms-spotty` JSON-RPC daemon, scanner CLI, and drop-in helper
//! library.

pub mod cli;
pub mod daemon;
pub mod lifecycle;
pub mod rpc_protocol;

pub use crate::cli::{run_cli, CliCommand, ScanCommandArgs, StreamCommandArgs};
pub use crate::daemon::{DaemonConfig, DaemonState, SpottyDaemon};
pub use crate::lifecycle::{LifecycleConfig, LifecycleSupervisor, SweepReport};
pub use crate::rpc_protocol::{
    constant_time_eq, default_endpoint_path, generate_bearer_token, read_endpoint_file,
    remove_endpoint_file, write_endpoint_file, DaemonEndpointInfo, JsonRpcError, JsonRpcRequest,
    JsonRpcResponse, DEFAULT_ENDPOINT_FILENAME, ERR_INTERNAL, ERR_INVALID_PARAMS,
    ERR_INVALID_REQUEST, ERR_METHOD_NOT_FOUND, ERR_PARSE, ERR_UNAUTHORIZED, SPOTTY_VERSION,
};
