use coderun::code_runner_server::CodeRunner;
use coderun::{RunCodeRequest, RunCodeResponse, RunStatus};
use tonic::{Request, Response, Status, transport::Server};
pub mod coderun {
    tonic::include_proto!("coderun"); // The string specified here must match the proto package name
}

use libc::{RLIMIT_AS, RLIMIT_CPU, rlimit, setrlimit};
use std::{
    io::{Read, Write},
    os::unix::process::{CommandExt, ExitStatusExt},
    process::{Command, Stdio},
    time::Instant,
};

use wait4::Wait4;

use crate::coderun::code_runner_server::CodeRunnerServer;

const CODE_DIR: &str = "/var/tmp/code-runner";

#[derive(Debug, Default)]
pub struct MyCodeRunner {}

#[tonic::async_trait]
impl CodeRunner for MyCodeRunner {
    async fn run_code(
        &self,
        request: Request<RunCodeRequest>,
    ) -> Result<Response<RunCodeResponse>, Status> {
        let req = request.into_inner();

        let code_dir = format!(
            "{}/{}",
            CODE_DIR,
            req.session.unwrap_or_else(|| (0..20)
                .map(|_| fastrand::alphanumeric())
                .collect::<String>())
        );
        std::fs::create_dir_all(&code_dir).expect("Failed to create code directory");

        for file in req.files {
            let file_path = format!("{}/{}", code_dir, file.name);
            std::fs::write(file_path, file.content).expect("Failed to write file");
        }

        let mut command = Command::new(req.program);

        command.current_dir(&code_dir);

        command.args(req.args);

        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        if let Some(limits) = req.limits {
            unsafe {
                command.pre_exec(move || {
                    let limit = rlimit {
                        rlim_cur: 1024 * 1024 * limits.max_memory,
                        rlim_max: 1024 * 1024 * limits.max_memory,
                    };
                    let cpu_limit = rlimit {
                        rlim_cur: limits.max_runtime,
                        rlim_max: limits.max_runtime,
                    };

                    if setrlimit(RLIMIT_AS, &limit) != 0 {
                        eprintln!("warning: failed to set memory limit");
                    }

                    if setrlimit(RLIMIT_CPU, &cpu_limit) != 0 {
                        eprintln!("warning: failed to set CPU time limit");
                    }

                    Ok(())
                });
            }
        }

        let start = Instant::now();

        let resources_result = match command.spawn() {
            Ok(mut child) => {
                if let Some(input_data) = req.input {
                    let mut stdin = child.stdin.take().expect("failed to open stdin");
                    if let Err(_) = stdin.write_all(&input_data) {
                        eprintln!("warning: failed to write to stdin, process could be dead");
                    }
                    drop(stdin);
                }

                Ok((
                    child.wait4(),
                    child.stdout.take().expect("expected stdout to be piped"),
                    child.stderr.take().expect("expected stderr to be piped"),
                ))
            }
            Err(e) => Err(e),
        };

        let elapsed = start.elapsed();

        if req.cleanup.unwrap_or_else(|| true) {
            if let Err(e) = std::fs::remove_dir_all(&code_dir) {
                eprintln!("warning: failed to clean up code directory: {}", e);
            }
        }

        match resources_result {
            Ok(resources) => {
                let (res_use, stdout, stderr) = resources;

                let res_use = match res_use {
                    Ok(r) => r,
                    Err(e) => {
                        // return Err(e);
                        eprintln!("warning: failed to wait for child process: {}", e);
                        return Err(Status::from_error(Box::new(e)));
                    }
                };

                let output = stdout
                    .bytes()
                    .map(|b| b.expect("failed to read stdout"))
                    .collect::<Vec<u8>>();

                let error_output = stderr
                    .bytes()
                    .map(|b| b.expect("failed to read stderr"))
                    .collect::<Vec<u8>>();

                Ok(Response::new(RunCodeResponse {
                    stdout: output,
                    stderr: error_output,
                    status: if res_use.status.success() {
                        RunStatus::Success.into()
                    } else if res_use.status.signal() == Some(9) {
                        RunStatus::TimeLimitExceeded.into()
                    } else {
                        RunStatus::RuntimeError.into()
                    },
                    runtime: res_use.rusage.utime.as_millis() as u64
                        + res_use.rusage.stime.as_millis() as u64,
                    walltime: elapsed.as_millis() as u64,
                    memory: res_use.rusage.maxrss,
                }))
            }
            Err(e) => {
                eprintln!("error: failed to run command: {}", e);
                Err(Status::from_error(Box::new(e)))
            }
        }
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
