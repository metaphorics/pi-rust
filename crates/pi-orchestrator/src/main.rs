//! `pi-orchestrator` binary entry point (port of cli.ts).
//!
//! Help text carries the Rust binary name (plan R7); everything else —
//! subcommands, flag handling, JSON output shape, exit codes — matches the
//! oracle CLI byte for byte.

use std::process::ExitCode;
use std::sync::Arc;

use pi_orchestrator::config;
use pi_orchestrator::ipc::{OrchestratorRequest, OrchestratorResponse, send_ipc_request};
use pi_orchestrator::radius::{RadiusPresence, get_radius_orchestrator_base_url};
use pi_orchestrator::serve::{ServeOptions, start};
use pi_orchestrator::storage::Storage;
use pi_orchestrator::wire::RpcCommandEnvelope;

fn print_help() {
    println!(
        "pi-orchestrator v{version}\n\nUsage:\n  pi-orchestrator serve\n  pi-orchestrator list\n  pi-orchestrator spawn [--cwd <path>] [--label <label>]\n  pi-orchestrator status <instance-id>\n  pi-orchestrator stop <instance-id>\n  pi-orchestrator rpc <instance-id> <json-command>\n  pi-orchestrator rpc-stream <instance-id>\n  pi-orchestrator --help\n  pi-orchestrator --version\n\nRPC stream stdin expects JSONL RpcCommand or extension_ui_response messages.",
        version = config::VERSION,
    );
}

fn print_response(response: &OrchestratorResponse) {
    match serde_json::to_string_pretty(response) {
        Ok(text) => println!("{text}"),
        Err(error) => eprintln!("{error}"),
    }
}

/// Port of cli.ts `getFlagValue`: the argument following the flag, wherever
/// the flag appears.
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let index = args.iter().position(|arg| arg == flag)?;
    args.get(index + 1).map(String::as_str)
}

async fn request_and_print(request: OrchestratorRequest) -> ExitCode {
    match send_ipc_request(&request).await {
        Ok(response) => {
            print_response(&response);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// Port of serve.ts's banner/exit shell around the lifecycle core in
/// `pi_orchestrator::serve`.
async fn serve() -> ExitCode {
    let socket_path = config::get_socket_path();
    let presence = match RadiusPresence::new() {
        Ok(presence) => Arc::new(presence),
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let radius_enabled = match presence.is_enabled().await {
        Ok(enabled) => enabled,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };

    let mut running = match start(ServeOptions {
        socket_path: socket_path.clone(),
        storage: Storage::from_config(),
        presence,
        label: None,
        spawn_command_override: None,
    })
    .await
    {
        Ok(running) => running,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };

    if radius_enabled {
        match get_radius_orchestrator_base_url() {
            Ok(base_url) => println!(
                "radius integration enabled: {} -> {base_url}",
                socket_path.display()
            ),
            Err(error) => eprintln!("{error}"),
        }
        if let Some(machine) = &running.machine {
            println!("radius machine id: {}", machine.id);
        }
    } else {
        println!(
            "radius integration disabled: login radius in ~/.pi/agent/auth.json or set PI_RADIUS_API_KEY"
        );
    }
    println!("orchestrator listening on {}", socket_path.display());

    let signal_result = wait_for_shutdown_signal().await;
    let shutdown_result = running.shutdown().await;
    match (signal_result, shutdown_result) {
        (Ok(()), Ok(())) => ExitCode::SUCCESS,
        (Err(error), _) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
        (_, Err(error)) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = interrupt.recv() => {}
        _ = terminate.recv() => {}
    }
    Ok(())
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}

/// Port of cli.ts `rpcStream`: raw socket passthrough. Socket bytes stream to
/// stdout verbatim (rpc_ready included); stdin JSONL lines are parsed and
/// re-encoded compactly to the socket. Socket end exits 0; socket errors and
/// invalid stdin JSON exit 1 (the oracle crashes on the latter). Stdin EOF
/// leaves the connection open, matching node's idle stdin.
#[cfg(unix)]
async fn rpc_stream(instance_id: String) -> ExitCode {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let stream = match UnixStream::connect(config::get_socket_path()).await {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let (mut read_half, mut write_half) = stream.into_split();
    let request = OrchestratorRequest::RpcStream {
        instance_id: instance_id.clone(),
    };
    let encoded = match pi_orchestrator::ipc::encode_message(&request) {
        Ok(encoded) => encoded,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = write_half.write_all(encoded.as_bytes()).await {
        eprintln!("{error}");
        return ExitCode::FAILURE;
    }
    eprintln!(
        "connected to rpc stream {instance_id}; send JSONL RpcCommand or extension_ui_response messages on stdin"
    );

    let mut socket_to_stdout = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let mut buffer = [0u8; 8192];
        loop {
            match read_half.read(&mut buffer).await {
                Ok(0) => return ExitCode::SUCCESS,
                Ok(read) => {
                    if stdout.write_all(&buffer[..read]).await.is_err() {
                        return ExitCode::FAILURE;
                    }
                    let _ = stdout.flush().await;
                }
                Err(error) => {
                    eprintln!("{error}");
                    return ExitCode::FAILURE;
                }
            }
        }
    });

    // On stdin EOF the write half is returned (not dropped) so the socket
    // stays fully open while events keep streaming, as in node.
    let mut stdin_to_socket = tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let parsed: serde_json::Value = match serde_json::from_str(line) {
                        Ok(parsed) => parsed,
                        Err(error) => {
                            eprintln!("{error}");
                            return Err(ExitCode::FAILURE);
                        }
                    };
                    let encoded = match pi_orchestrator::ipc::encode_message(&parsed) {
                        Ok(encoded) => encoded,
                        Err(error) => {
                            eprintln!("{error}");
                            return Err(ExitCode::FAILURE);
                        }
                    };
                    if let Err(error) = write_half.write_all(encoded.as_bytes()).await {
                        eprintln!("{error}");
                        return Err(ExitCode::FAILURE);
                    }
                }
                Ok(None) => return Ok(write_half),
                Err(error) => {
                    eprintln!("{error}");
                    return Err(ExitCode::FAILURE);
                }
            }
        }
    });

    let code = tokio::select! {
        code = &mut socket_to_stdout => code.unwrap_or(ExitCode::FAILURE),
        result = &mut stdin_to_socket => match result {
            Ok(Ok(_write_half_kept_open)) => {
                // stdin closed; keep streaming until the socket ends.
                (&mut socket_to_stdout).await.unwrap_or(ExitCode::FAILURE)
            }
            Ok(Err(code)) => code,
            Err(_) => ExitCode::FAILURE,
        },
    };
    socket_to_stdout.abort();
    stdin_to_socket.abort();
    code
}

#[cfg(not(unix))]
async fn rpc_stream(_instance_id: String) -> ExitCode {
    eprintln!("Unix socket IPC is not supported on this platform");
    ExitCode::FAILURE
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let Some(command) = args.first().map(String::as_str) else {
        print_help();
        return ExitCode::SUCCESS;
    };

    match command {
        "--help" | "-h" => {
            print_help();
            ExitCode::SUCCESS
        }
        "--version" | "-v" => {
            println!("{}", config::VERSION);
            ExitCode::SUCCESS
        }
        "serve" => serve().await,
        "list" => request_and_print(OrchestratorRequest::List).await,
        "spawn" => {
            let cwd = match flag_value(&args, "--cwd") {
                Some(cwd) => cwd.to_owned(),
                None => match std::env::current_dir() {
                    Ok(cwd) => cwd.to_string_lossy().into_owned(),
                    Err(error) => {
                        eprintln!("{error}");
                        return ExitCode::FAILURE;
                    }
                },
            };
            let label = flag_value(&args, "--label").map(str::to_owned);
            request_and_print(OrchestratorRequest::Spawn {
                cwd,
                label,
                provider: None,
                model: None,
            })
            .await
        }
        "status" => {
            let Some(instance_id) = args.get(1) else {
                eprintln!("Usage: pi-orchestrator status <instance-id>");
                return ExitCode::FAILURE;
            };
            request_and_print(OrchestratorRequest::Status {
                instance_id: instance_id.clone(),
            })
            .await
        }
        "stop" => {
            let Some(instance_id) = args.get(1) else {
                eprintln!("Usage: pi-orchestrator stop <instance-id>");
                return ExitCode::FAILURE;
            };
            request_and_print(OrchestratorRequest::Stop {
                instance_id: instance_id.clone(),
            })
            .await
        }
        "rpc" => {
            let (Some(instance_id), Some(command_json)) = (args.get(1), args.get(2)) else {
                eprintln!("Usage: pi-orchestrator rpc <instance-id> <json-command>");
                return ExitCode::FAILURE;
            };
            let value: serde_json::Value = match serde_json::from_str(command_json) {
                Ok(value) => value,
                Err(error) => {
                    eprintln!("{error}");
                    return ExitCode::FAILURE;
                }
            };
            let command = match RpcCommandEnvelope::try_from(value) {
                Ok(command) => command,
                Err(error) => {
                    eprintln!("{error}");
                    return ExitCode::FAILURE;
                }
            };
            request_and_print(OrchestratorRequest::Rpc {
                instance_id: instance_id.clone(),
                command,
            })
            .await
        }
        "rpc-stream" => {
            let Some(instance_id) = args.get(1) else {
                eprintln!("Usage: pi-orchestrator rpc-stream <instance-id>");
                return ExitCode::FAILURE;
            };
            rpc_stream(instance_id.clone()).await
        }
        other => {
            eprintln!("Unknown command: {other}");
            print_help();
            ExitCode::FAILURE
        }
    }
}
