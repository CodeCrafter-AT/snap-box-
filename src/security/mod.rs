use std::{
    collections::HashMap,
    fs,
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use tokio::process::Command;

use crate::{
    config::{ResourceConfig, SecurityConfig},
    errors::{Result, SnapBoxError},
    sandbox::{CommandRequest, CommandResult},
};

const BLOCKED_ENV_VARS: &[&str] = &[
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "SSH_AUTH_SOCK",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
];

const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Resolves an optional relative working directory strictly inside `workspace`,
/// rejecting absolute paths, `..` traversal, or symlink escapes outside `workspace`.
pub fn resolve_confined_cwd(workspace: &Path, cwd_rel: Option<&str>) -> Result<PathBuf> {
    let canonical_ws = fs::canonicalize(workspace).map_err(|e| SnapBoxError::Filesystem {
        operation: "canonicalize".into(),
        path: workspace.display().to_string(),
        source: e,
    })?;

    let Some(rel_str) = cwd_rel else {
        return Ok(canonical_ws);
    };

    let rel_path = Path::new(rel_str);
    if rel_path.is_absolute() {
        return Err(SnapBoxError::SecurityViolation(format!(
            "absolute cwd '{}' is forbidden inside sandbox",
            rel_str
        )));
    }
    for comp in rel_path.components() {
        if matches!(
            comp,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(SnapBoxError::SecurityViolation(format!(
                "path traversal component in cwd '{}' is forbidden",
                rel_str
            )));
        }
    }

    let candidate = canonical_ws.join(rel_path);
    let canonical_target = fs::canonicalize(&candidate).map_err(|e| {
        SnapBoxError::Validation(format!(
            "cwd '{}' does not exist inside workspace: {}",
            rel_str, e
        ))
    })?;

    if !canonical_target.starts_with(&canonical_ws) {
        return Err(SnapBoxError::SecurityViolation(format!(
            "cwd '{}' escaped sandbox workspace via symlink",
            rel_str
        )));
    }

    Ok(canonical_target)
}

/// Validates that a relative file path inside a sandbox stays strictly confined to `workspace`.
pub fn validate_workspace_relative_path(workspace: &Path, rel: &Path) -> Result<PathBuf> {
    if rel.is_absolute() {
        return Err(SnapBoxError::SecurityViolation(format!(
            "absolute path '{}' is not permitted inside sandbox",
            rel.display()
        )));
    }
    for comp in rel.components() {
        if matches!(
            comp,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(SnapBoxError::SecurityViolation(format!(
                "parent traversal in '{}' is not permitted",
                rel.display()
            )));
        }
    }
    let canonical_ws = fs::canonicalize(workspace).map_err(|e| SnapBoxError::Filesystem {
        operation: "canonicalize".into(),
        path: workspace.display().to_string(),
        source: e,
    })?;
    let joined = canonical_ws.join(rel);
    if joined.exists() {
        let resolved = fs::canonicalize(&joined).map_err(|e| SnapBoxError::Filesystem {
            operation: "canonicalize".into(),
            path: joined.display().to_string(),
            source: e,
        })?;
        if !resolved.starts_with(&canonical_ws) {
            return Err(SnapBoxError::SecurityViolation(format!(
                "symlink '{}' escapes sandbox root",
                rel.display()
            )));
        }
    }
    Ok(joined)
}

/// Builds a sanitized environment map preventing host secret leakage and loader injection.
pub fn build_sanitized_env(
    workspace: &Path,
    extra_env: &HashMap<String, String>,
    security: &SecurityConfig,
) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();

    if !security.strip_host_env {
        for (k, v) in std::env::vars() {
            if !BLOCKED_ENV_VARS.contains(&k.as_str()) {
                out.insert(k, v);
            }
        }
    } else {
        let home_dir = workspace.join(".sandbox_home");
        let tmp_dir = workspace.join(".sandbox_tmp");
        let _ = fs::create_dir_all(&home_dir);
        let _ = fs::create_dir_all(&tmp_dir);

        out.insert(
            "PATH".into(),
            "/usr/local/bin:/usr/bin:/bin:/opt/homebrew/bin".into(),
        );
        out.insert("LANG".into(), "C.UTF-8".into());
        out.insert("TERM".into(), "xterm-256color".into());
        out.insert("HOME".into(), home_dir.display().to_string());
        out.insert("TMPDIR".into(), tmp_dir.display().to_string());
        out.insert("SNAP_BOX_SANDBOX".into(), "1".into());
    }

    for (k, v) in extra_env {
        if BLOCKED_ENV_VARS
            .iter()
            .any(|blocked| blocked.eq_ignore_ascii_case(k))
        {
            return Err(SnapBoxError::SecurityViolation(format!(
                "environment variable '{k}' is blocked by security policy"
            )));
        }
        out.insert(k.clone(), v.clone());
    }

    Ok(out)
}

/// Executes a command inside the sandbox workspace with process-group tracking,
/// sanitized environment, optional rlimits, and wall-clock timeout enforcement.
pub async fn execute_sandboxed_command(
    workspace: &Path,
    req: &CommandRequest,
    security: &SecurityConfig,
    resources: &ResourceConfig,
) -> Result<CommandResult> {
    if req.program.trim().is_empty() {
        return Err(SnapBoxError::Validation("program must not be empty".into()));
    }

    let cwd = if security.enforce_path_confinement {
        resolve_confined_cwd(workspace, req.cwd_rel.as_deref())?
    } else {
        workspace.to_path_buf()
    };

    let sanitized_env = build_sanitized_env(workspace, &req.env, security)?;
    let timeout_ms = req
        .timeout_ms
        .unwrap_or(resources.execution_timeout_ms)
        .min(resources.execution_timeout_ms);

    let mut cmd = build_platform_command(workspace, req, security)?;
    cmd.current_dir(&cwd);
    cmd.env_clear();
    cmd.envs(&sanitized_env);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let enforce_rlimits = security.enforce_rlimits;
    let max_cpu = resources.max_cpu_seconds;
    let max_fsize = resources.max_disk_bytes;
    let max_nofile = resources.max_open_files;
    let enable_no_new_privs = security.linux_enable_namespaces;

    // SAFETY: `pre_exec` executes only async-signal-safe POSIX calls before `execve`.
    unsafe {
        cmd.pre_exec(move || {
            // Isolate child in its own process group so timeout/release kills all descendants.
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }

            if enforce_rlimits {
                apply_rlimit(libc::RLIMIT_CPU, max_cpu)?;
                apply_rlimit(libc::RLIMIT_FSIZE, max_fsize)?;
                apply_rlimit(libc::RLIMIT_NOFILE, max_nofile)?;
            }

            #[cfg(target_os = "linux")]
            if enable_no_new_privs {
                let _ = libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
            }
            #[cfg(not(target_os = "linux"))]
            let _ = enable_no_new_privs;

            Ok(())
        });
    }

    let started = Instant::now();
    let child = cmd.spawn().map_err(|e| {
        SnapBoxError::ExecutionFailed(format!("failed to spawn '{}': {e}", req.program))
    })?;

    let pid = child.id();
    let timeout = Duration::from_millis(timeout_ms);

    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let duration_ms = started.elapsed().as_millis() as u64;
            Ok(CommandResult {
                exit_code: output.status.code(),
                timed_out: false,
                stdout: truncate_utf8(&output.stdout, MAX_OUTPUT_BYTES),
                stderr: truncate_utf8(&output.stderr, MAX_OUTPUT_BYTES),
                duration_ms,
            })
        }
        Ok(Err(e)) => Err(SnapBoxError::ExecutionFailed(format!(
            "process wait failed: {e}"
        ))),
        Err(_) => {
            if let Some(raw_pid) = pid {
                kill_process_group(raw_pid);
            }
            let duration_ms = started.elapsed().as_millis() as u64;
            Ok(CommandResult {
                exit_code: None,
                timed_out: true,
                stdout: String::new(),
                stderr: format!("command timed out after {timeout_ms}ms; process group killed"),
                duration_ms,
            })
        }
    }
}

fn build_platform_command(
    workspace: &Path,
    req: &CommandRequest,
    security: &SecurityConfig,
) -> Result<Command> {
    #[cfg(target_os = "macos")]
    if security.macos_seatbelt_profile && Path::new("/usr/bin/sandbox-exec").exists() {
        let canonical_ws = fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
        let net_rule = if security.allow_network {
            "(allow network*)"
        } else {
            "(deny network*)"
        };
        let profile = format!(
            "(version 1)\n(allow default)\n{net_rule}\n(deny file-write* (subpath \"/\"))\n(allow file-write* (subpath \"{}\") (subpath \"/private/tmp\") (subpath \"/dev/null\"))\n",
            canonical_ws.display()
        );
        let mut cmd = Command::new("/usr/bin/sandbox-exec");
        cmd.arg("-p").arg(profile).arg(&req.program).args(&req.args);
        return Ok(cmd);
    }

    #[cfg(not(target_os = "macos"))]
    let _ = (workspace, security);

    let mut cmd = Command::new(&req.program);
    cmd.args(&req.args);
    Ok(cmd)
}

unsafe fn apply_rlimit(resource: libc::c_int, limit: u64) -> std::io::Result<()> {
    let mut cur: libc::rlimit = std::mem::zeroed();
    if libc::getrlimit(resource, &mut cur) != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let capped = (limit as libc::rlim_t).min(cur.rlim_max);
    let new_lim = libc::rlimit {
        rlim_cur: capped,
        rlim_max: capped,
    };
    if libc::setrlimit(resource, &new_lim) != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Terminates an entire process group (best-effort) using `killpg(pid, SIGKILL)`.
pub fn kill_process_group(pgid: u32) {
    if pgid == 0 {
        return;
    }
    // SAFETY: Killing a child process group created with `setpgid(0, 0)`.
    unsafe {
        libc::killpg(pgid as libc::pid_t, libc::SIGKILL);
    }
}

fn truncate_utf8(bytes: &[u8], max_bytes: usize) -> String {
    if bytes.len() <= max_bytes {
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        let mut s = String::from_utf8_lossy(&bytes[..max_bytes]).into_owned();
        s.push_str("\n...[output truncated]");
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_traversal_is_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        let err = resolve_confined_cwd(tmp.path(), Some("../../etc")).unwrap_err();
        assert!(matches!(err, SnapBoxError::SecurityViolation(_)));
    }

    #[test]
    fn absolute_cwd_is_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        let err = resolve_confined_cwd(tmp.path(), Some("/etc")).unwrap_err();
        assert!(matches!(err, SnapBoxError::SecurityViolation(_)));
    }

    #[test]
    fn blocked_loader_env_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut extra = HashMap::new();
        extra.insert("DYLD_INSERT_LIBRARIES".into(), "/tmp/evil.dylib".into());
        let err = build_sanitized_env(tmp.path(), &extra, &SecurityConfig::default()).unwrap_err();
        assert!(matches!(err, SnapBoxError::SecurityViolation(_)));
    }

    #[test]
    fn host_secrets_are_stripped() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("SNAPBOX_TEST_AWS_SECRET", "topsecret");
        let env =
            build_sanitized_env(tmp.path(), &HashMap::new(), &SecurityConfig::default()).unwrap();
        assert!(!env.contains_key("SNAPBOX_TEST_AWS_SECRET"));
        assert_eq!(env.get("SNAP_BOX_SANDBOX").map(String::as_str), Some("1"));
    }
}
