use std::fs;
use std::io::{Read, Write};

use hakoniwa::seccomp::{Action, Arch, Filter};
use hakoniwa::{Container, Namespace, Rlimit, Runctl, Stdio};

use crate::types::{Limit, RunOutput, RunStatus};

pub struct Runner {
    container: Container,
    path: String,
}

const BANNED_SYSCALLS: &[&str] = &[
    "mount", "umount", "poweroff", "reboot", "socket", "bind", "connect", "listen", "sendto",
    "recvfrom",
];

impl Runner {
    pub fn new(code_path: String) -> Self {
        fs::create_dir_all(&code_path).expect("Failed to create code directory");
        let mut container = Container::new();

        let mut filter = Filter::new(Action::Allow);

        #[cfg(target_arch = "x86_64")]
        {
            filter.add_arch(Arch::X8664);
            filter.add_arch(Arch::X86);
            filter.add_arch(Arch::X32);
        }

        container
            .unshare(Namespace::Cgroup)
            .unshare(Namespace::Ipc)
            .unshare(Namespace::Uts)
            .unshare(Namespace::Network);

        BANNED_SYSCALLS.iter().for_each(|syscall| {
            filter.add_rule(Action::Errno(libc::SIGSYS), syscall);
        });

        container.rootfs("/").expect("unable to mount root fs");
        container.seccomp_filter(filter);

        container.bindmount_rw(&code_path, "/box");
        container.runctl(Runctl::GetProcPidStatus);
        container.runctl(Runctl::GetProcPidSmapsRollup);

        Self {
            container,
            path: code_path.to_string(),
        }
    }

    pub fn put_file(&mut self, file_path: String, content: &[u8]) -> Result<(), std::io::Error> {
        let file = std::fs::File::create(format!("{}/{}", self.path, file_path))?;
        let mut writer = std::io::BufWriter::new(file);
        writer.write_all(content)?;
        writer.flush()?;
        Ok(())
    }

    pub fn execute_program(
        &mut self,
        program: &str,
        args: Vec<String>,
        limit: Option<Limit>,
        stdin: Option<Vec<u8>>,
    ) -> RunOutput {
        let walltime: Option<u64>;
        if let Some(limit) = limit {
            if let Some(time_limit) = limit.time_limit {
                self.container
                    .setrlimit(Rlimit::Cpu, time_limit, time_limit);
            }

            if let Some(memory_limit) = limit.memory {
                self.container
                    .setrlimit(Rlimit::As, memory_limit, memory_limit);
            }

            walltime = limit.walltime_limit;
        } else {
            walltime = None;
        }

        let mut cmd = self.container.command(program);
        cmd.current_dir("/box")
            .args(args)
            .env("PATH", "/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // cmd.wait_timeout(walltime);
        if walltime.is_some() {
            cmd.wait_timeout(walltime.unwrap());
        }

        let mut proc = match cmd.spawn() {
            Ok(p) => p,
            Err(_) => return RunOutput::error("Failed to spawn process".to_string(), None, None),
        };

        if let Some(stdin) = stdin {
            if let Some(mut proc_stdin) = proc.stdin.take() {
                if let Err(_) = proc_stdin.write_all(&stdin) {
                    // return RunOutput::error("Failed to write to stdin".to_string(), None, None);
                    eprintln!("warning: failed to write to stdin, process could be dead");
                }
                drop(proc_stdin);
            } else {
                return RunOutput::error("Failed to take stdin".to_string(), None, None);
            }
        }

        let output = match proc.wait_with_output() {
            Ok(o) => o,
            Err(_) => {
                return RunOutput::error("Failed to wait for process".to_string(), None, None);
            }
        };

        let output_status = output.status.clone();

        let resource = match output.status.rusage {
            Some(r) => r,
            None => {
                eprintln!("Failed to get resource usage: {}", output_status.reason);
                return RunOutput::error(
                    "Failed to get resource usage".to_string(),
                    Some(output.stderr),
                    Some(output.stdout),
                );
            }
        };

        let proc_resource = match output.status.proc_pid_status {
            Some(r) => r,
            None => {
                eprintln!(
                    "Failed to get process resource usage: {}",
                    output_status.reason
                );
                return RunOutput::error(
                    "Failed to get process resource usage".to_string(),
                    Some(output.stderr),
                    Some(output.stdout),
                );
            }
        };

        // output.status
        let status = match output_status.code {
            0 => RunStatus::Success,
            137 | 152 => RunStatus::TimeLimitExceeded,
            // 125 => RunStatus::SecurityViolation,
            _ => RunStatus::RuntimeError(output_status.reason),
        };

        RunOutput {
            stdout: output.stdout,
            stderr: output.stderr,
            runtime: resource.user_time.as_millis() + resource.system_time.as_millis(),
            memory_usage: proc_resource.vmrss as i64,
            status,
            exit_code: Some(output_status.code),
        }
    }

    pub fn get_file(&mut self, file_path: String) -> Result<Vec<u8>, std::io::Error> {
        let file = std::fs::File::open(format!("{}/{}", self.path, file_path))?;
        let mut reader = std::io::BufReader::new(file);
        let mut buffer = Vec::new();
        reader.read_to_end(&mut buffer)?;
        Ok(buffer)
    }

    pub fn cleanup(&mut self) -> Result<(), std::io::Error> {
        std::fs::remove_dir_all(&self.path)?;
        Ok(())
    }
}
