mod runner;
mod types;

use coderun::code_runner_server::CodeRunner;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};
use tonic::Streaming;
use tonic::{Request, Response, Status, transport::Server};
pub mod coderun {
    tonic::include_proto!("coderun"); // The string specified here must match the proto package name
}

use crate::coderun::code_runner_server::CodeRunnerServer;
use crate::coderun::command_request::Command;
use crate::coderun::{
    CommandRequest, CommandResponse, GetFileResponse, PutFileResponse, RunCodeResponse, RunStatus,
    command_response,
};
use crate::runner::Runner;
use crate::types::Limit;
use std::error::Error;
use std::io::ErrorKind;
use std::pin::Pin;

const CODE_DIR: &str = "/var/tmp/code-runner";

type CommandResult<T> = Result<Response<T>, Status>;
type ResponseStream = Pin<Box<dyn Stream<Item = Result<CommandResponse, Status>> + Send>>;

fn match_for_io_error(err_status: &Status) -> Option<&std::io::Error> {
    let mut err: &(dyn Error + 'static) = err_status;

    loop {
        if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
            return Some(io_err);
        }

        // h2::Error do not expose std::io::Error with `source()`
        // https://github.com/hyperium/h2/pull/462
        if let Some(h2_err) = err.downcast_ref::<h2::Error>() {
            if let Some(io_err) = h2_err.get_io() {
                return Some(io_err);
            }
        }

        err = err.source()?;
    }
}

#[derive(Debug, Default)]
pub struct MyCodeRunner {}

#[tonic::async_trait]
impl CodeRunner for MyCodeRunner {
    type StartSessionStream = ResponseStream;

    async fn start_session(
        &self,
        req: Request<Streaming<CommandRequest>>,
    ) -> CommandResult<Self::StartSessionStream> {
        let mut in_stream = req.into_inner();
        let (tx, rx) = mpsc::channel(128);

        println!("session started");

        tokio::spawn(async move {
            let session_id = (0..20)
                .map(|_| fastrand::alphanumeric())
                .collect::<String>();
            let mut runner = Runner::new(format!("{}/{}", CODE_DIR, session_id));

            while let Some(result) = in_stream.next().await {
                match result {
                    Ok(v) => {
                        let command = v
                            .command
                            .ok_or_else(|| {
                                Status::invalid_argument("CommandRequest must contain a command")
                            })
                            .expect("CommandRequest must contain a command");
                        // coderun::command_request::Command
                        match command {
                            Command::Put(put) => {
                                let file_path = put.filename;
                                let content = put.content;

                                if let Err(err) = runner.put_file(file_path, &content) {
                                    tx.send(Err(Status::internal(format!(
                                        "Failed to put file: {}",
                                        err
                                    ))))
                                    .await
                                    .unwrap();
                                    continue;
                                }

                                tx.send(Ok(CommandResponse {
                                    response: Some(command_response::Response::Put(
                                        PutFileResponse {
                                            length: content.len() as u32,
                                        },
                                    )),
                                    ..Default::default()
                                }))
                                .await
                                .unwrap();
                            }
                            Command::Run(run) => {
                                let run_command = run.command;
                                let limits = run.limits;
                                let stdin = run.input;

                                let limit = if let Some(limits) = limits {
                                    Some(Limit {
                                        memory: Some(limits.max_memory),
                                        time_limit: Some(limits.max_runtime),
                                        walltime_limit: Some(limits.max_runtime * 2),
                                    })
                                } else {
                                    None
                                };

                                let output = runner.execute_program(
                                    "/usr/bin/sh",
                                    vec!["-c".to_string(), run_command],
                                    limit,
                                    stdin,
                                );

                                tx.send(Ok(CommandResponse {
                                    response: Some(command_response::Response::Run(
                                        RunCodeResponse {
                                            stdout: output.stdout,
                                            stderr: output.stderr,
                                            status: match output.status {
                                                types::RunStatus::Success => {
                                                    RunStatus::Success.into()
                                                }
                                                types::RunStatus::TimeLimitExceeded => {
                                                    RunStatus::TimeLimitExceeded.into()
                                                }

                                                types::RunStatus::SystemError(_) => {
                                                    RunStatus::SystemError.into()
                                                }

                                                types::RunStatus::RuntimeError(_) => {
                                                    RunStatus::RuntimeError.into()
                                                }
                                            },
                                            runtime: output.runtime as u64,
                                            memory: output.memory_usage as u64,
                                            exit_code: output.exit_code,
                                        },
                                    )),
                                    ..Default::default()
                                }))
                                .await
                                .unwrap();
                            }
                            Command::Get(get) => {
                                let file_path = get.filename;

                                match runner.get_file(file_path) {
                                    Ok(content) => {
                                        tx.send(Ok(CommandResponse {
                                            response: Some(command_response::Response::Get(
                                                GetFileResponse {
                                                    content: content.clone(),
                                                },
                                            )),
                                            ..Default::default()
                                        }))
                                        .await
                                        .unwrap();
                                    }
                                    Err(err) => {
                                        tx.send(Err(Status::internal(format!(
                                            "Failed to get file: {}",
                                            err
                                        ))))
                                        .await
                                        .unwrap();
                                    }
                                }
                            }
                        }
                    }
                    Err(err) => {
                        if let Some(io_err) = match_for_io_error(&err) {
                            if io_err.kind() == ErrorKind::BrokenPipe {
                                eprintln!("\tclient disconnected: broken pipe");
                                break;
                            }
                        }

                        match tx.send(Err(err)).await {
                            Ok(_) => (),
                            Err(_err) => break, // response was dropped
                        }
                    }
                }
            }

            // clean up remove the session directory
            if let Err(err) = runner.cleanup() {
                eprintln!("Failed to clean up session directory: {}", err);
            }

            print!("session {} ended\n", session_id);
        });

        let out_stream = ReceiverStream::new(rx);

        Ok(Response::new(
            Box::pin(out_stream) as Self::StartSessionStream
        ))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "[::1]:50051".parse()?;
    let runner = MyCodeRunner::default();

    println!("code server listening on {}", addr);
    Server::builder()
        .add_service(CodeRunnerServer::new(runner))
        .serve(addr)
        .await?;

    Ok(())
}
