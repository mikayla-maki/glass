# Docker Sandbox — Implementation Plan

**Module:** `src/sandbox/`
**Responsibility:** Manage the Docker container that provides air-gapped shell execution for the agent.

---

## Design Decision: Docker CLI via `tokio::process::Command`

The spec calls for managing Docker via `Command` / docker CLI. While the `bollard` crate (v0.20) provides a Rust-native Docker API client, using the Docker CLI directly is simpler, has fewer dependencies, and is easier to debug. Glass only needs three operations: create a container, exec a command, and remove a container.

If complexity grows, `bollard` is a reasonable migration target.

---

## Sandbox Image

```dockerfile
# Dockerfile.sandbox
FROM ubuntu:24.04

RUN apt-get update && apt-get install -y \
    python3 \
    python3-pip \
    nodejs \
    npm \
    jq \
    curl \
    git \
    ripgrep \
    tree \
    vim-tiny \
    && rm -rf /var/lib/apt/lists/*

# Non-root user for safety
RUN useradd -m -s /bin/bash glass
USER glass
WORKDIR /workspace
```

Build once: `docker build -t glass-sandbox -f Dockerfile.sandbox .`

### Image Contents

| Category | Packages | Rationale |
|----------|----------|-----------|
| **Shell** | bash, coreutils | Foundation |
| **Text** | grep, sed, awk, jq, ripgrep | File search and manipulation |
| **Languages** | python3, node (LTS) | Script execution |
| **Files** | tree, file, zip, unzip | File management |
| **Dev** | git, vim-tiny | Workspace ops |

### What's NOT in the Image

- No compilers (gcc, rustc) — the agent doesn't need to build native code
- No package managers with network (pip, npm install) — no network anyway
- No browsers or GUI tools
- No sudo / root access

### Image Size Target

Under 500MB. The image is built once and reused for all invocations.

---

## Container Lifecycle

`DockerSandbox` implements the `Sandbox` trait. The MCP server's tool executor accepts `&dyn Sandbox`, never the concrete struct.

```rust
// sandbox/docker.rs

use std::sync::Mutex;

pub struct DockerSandbox {
    image: String,
    workspace_host_path: PathBuf,
    /// Active container ID, if any. Interior mutability so trait methods take &self.
    container_id: Mutex<Option<String>>,
}

#[async_trait]
impl Sandbox for DockerSandbox {
    async fn create_container(
        &self,
        project_workspace: &Path,
    ) -> Result<(), SandboxError> {
        let output = Command::new("docker")
            .args([
                "run", "-d",
                "--network", "none",
                "--name", &format!("glass-sandbox-{}", Uuid::new_v4()),
                "-v", &format!("{}:/workspace", project_workspace.display()),
                "-w", "/workspace",
                "--user", "glass",
                // Resource limits
                "--memory", "512m",
                "--cpus", "1.0",
                // No capabilities
                "--cap-drop", "ALL",
                &self.image,
                "sleep", "3600",  // Keep container alive
            ])
            .output()
            .await?;

        if !output.status.success() {
            return Err(SandboxError::CreateFailed(
                String::from_utf8_lossy(&output.stderr).to_string()
            ));
        }

        let container_id = String::from_utf8_lossy(&output.stdout)
            .trim().to_string();
        *self.container_id.lock().unwrap() = Some(container_id);
        Ok(())
    }

    async fn exec(
        &self,
        command: &str,
        timeout: Duration,
    ) -> Result<ExecResult, SandboxError> {
        let guard = self.container_id.lock().unwrap();
        let container_id = guard.as_ref()
            .ok_or(SandboxError::NoContainer)?;

        let child = Command::new("docker")
            .args([
                "exec",
                container_id,
                "/bin/bash", "-c", command,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let result = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| SandboxError::Timeout)?
            .map_err(SandboxError::from)?;

        Ok(ExecResult {
            stdout: String::from_utf8_lossy(&result.stdout).to_string(),
            stderr: String::from_utf8_lossy(&result.stderr).to_string(),
            exit_code: result.status.code().unwrap_or(-1),
        })
    }

    async fn destroy(&self) -> Result<(), SandboxError> {
        if let Some(id) = self.container_id.lock().unwrap().take() {
            let _ = Command::new("docker")
                .args(["rm", "-f", &id])
                .output()
                .await;
        }
        Ok(())
    }
}

pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}
```

---

## Sandbox Lifecycle Per Invocation

1. **Before the Claude Code session starts:** `sandbox.create_container(project_workspace)` — creates a new container with the appropriate workspace mounted.
2. **During the session:** Each `shell` MCP tool call runs `sandbox.exec(command, timeout)` inside the MCP server subprocess.
3. **After the session finishes:** `sandbox.destroy()` — removes the container.

Each invocation gets a fresh container. There's no persistent container state between invocations — the workspace volume persists, the container is disposable.

---

## Workspace Volume Mounting

The volume mount depends on the invocation type:

| Invocation Type | Mount Path |
|----------------|------------|
| Regular project | `workspace/{project}/` → `/workspace` |
| Root (owner present) | `workspace/` → `/workspace` (full access) |
| Root (autonomous) | `workspace/` → `/workspace` (but tools restrict to root-level files) |
| Subagent (query_projects) | `workspace/{project}/` → `/workspace` |

For root-autonomous invocations, the Docker mount includes the full workspace, but the `read_file`/`write_file` tools enforce that only root-level files (identity.md, skills/, knowledge/, inbox/) are accessible — not project subdirectories. This is tool-level enforcement, not Docker-level. The reason: the agent may need to run shell commands that operate on root-level files, and restricting the Docker mount would make this impossible.

---

## Container Security Hardening

```
docker run -d \
    --network none \              # No network access
    --cap-drop ALL \              # Drop all Linux capabilities
    --security-opt no-new-privileges \  # Can't escalate
    --read-only \                 # Root filesystem is read-only
    --tmpfs /tmp:size=100m \      # Writable /tmp with size limit
    -v workspace:/workspace \     # Only writable mount
    --memory 512m \               # Memory limit
    --cpus 1.0 \                  # CPU limit
    --pids-limit 256 \            # Process limit
    --user glass \                # Non-root user
    glass-sandbox \
    sleep 3600
```

---

## Container Pooling (Future Optimization)

MVP creates and destroys a container per invocation. If startup latency becomes an issue (~1-2s per container), we can pool warm containers:

1. Maintain a pool of 2-3 idle containers
2. Assign a container from the pool for each invocation
3. After invocation, reset the container state and return to pool
4. Monitor container health and replace stale ones

This is explicitly a future optimization, not MVP.