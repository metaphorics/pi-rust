pub mod client;
pub mod protocol;
pub mod server;

pub use client::{
    IpcClientError, RpcStreamClient, connect_rpc_stream, connect_rpc_stream_to, send_ipc_request,
    send_ipc_request_to,
};
pub use protocol::{
    InstanceSummary, OrchestratorRequest, OrchestratorResponse, ProtocolError, encode_message,
    parse_request_line, parse_response_line,
};
pub use server::{
    EventSendError, HandlerFuture, IpcRequestHandler, IpcServer, IpcServerError, RpcEventSink,
    RpcStreamHandler, start_ipc_server, start_ipc_server_at,
};
