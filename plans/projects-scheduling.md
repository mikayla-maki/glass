# Projects & Scheduling — Implementation Plan

Code samples and implementation details for project management and the scheduling system.

**Parent document:** [ARCHITECTURE.md](../ARCHITECTURE.md)

---

## Project Management

**Module:** `src/projects/`

### Project Discovery

At startup, the bot scans the workspace directory to discover projects. A directory is a project if it contains a `brief.md` file.

```rust
// projects/registry.rs

pub struct ProjectRegistry {
    projects: HashMap<String, Project>,
    workspace_root: PathBuf,
}

impl ProjectRegistry {
    /// Scan the workspace directory and discover all projects.
    /// A directory is a project if it contains a brief.md file.
    pub fn scan(workspace_root: &Path) -> Result<Self, ProjectError> {
        let mut projects = HashMap::new();

        for entry in std::fs::read_dir(workspace_root)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                let name = path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();

                // Skip root-level non-project directories
                if matches!(name.as_str(), "skills" | "knowledge" | "inbox") {
                    continue;
                }

                // A directory is a project if it has brief.md
                if path.join("brief.md").exists() {
                    projects.insert(name.clone(), Project {
                        name: name.clone(),
                        workspace_path: path.clone(),
                        channel_id: ChannelId::default(), // Resolved later via Discord
                        is_root: false,
                        archived: path.join(".archived").exists(),
                    });
                }
            }
        }

        Ok(Self { projects, workspace_root: workspace_root.to_path_buf() })
    }

    /// Resolve Discord channel IDs for all projects by matching channel names.
    pub async fn resolve_channels(
        &mut self,
        guild_id: GuildId,
        http: &Http,
    ) -> Result<(), ProjectError> { ... }
}
```

---

## Scheduling

**Module:** `src/scheduler/`

### Cron Loop Design

The scheduler runs as a background `tokio::spawn` task with a 30-second check interval (giving ±30s accuracy — fine for personal scheduling).

```rust
// scheduler/cron.rs

pub async fn run_scheduler(
    workspace_root: PathBuf,
    projects: Arc<RwLock<ProjectRegistry>>,
    invoke_tx: mpsc::Sender<InvocationRequest>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));

    loop {
        interval.tick().await;

        let now = Utc::now();

        // Reload all schedule files (agent may have modified them)
        let schedules = load_all_schedules(&workspace_root, &projects.read().await);

        for (project_name, task) in &schedules {
            if !task.enabled {
                continue;
            }

            if should_fire(&task.cron, now) {
                let _ = invoke_tx.send(InvocationRequest {
                    project_name: project_name.clone(),
                    trigger: InvocationTrigger::ScheduledTask {
                        task_id: task.id.clone(),
                        cron_expression: task.cron.clone(),
                        description: task.description.clone(),
                    },
                }).await;
            }
        }
    }
}
```

### Cron Parsing

Uses the `cron` crate for parsing cron expressions and determining next-fire times.

```rust
// scheduler/tasks.rs

/// Determine if a cron expression should fire at the given time.
/// Checks if `now` falls within the current 30-second check window.
pub fn should_fire(cron_expr: &str, now: DateTime<Utc>) -> bool {
    let schedule = cron::Schedule::from_str(cron_expr).ok();
    if let Some(schedule) = schedule {
        // Find the most recent scheduled time
        for datetime in schedule.after(&(now - chrono::Duration::seconds(30))) {
            if datetime <= now {
                return true;
            }
            break;
        }
    }
    false
}
```

### Schedule File Loading

```rust
// scheduler/cron.rs

fn load_all_schedules(
    workspace_root: &Path,
    projects: &ProjectRegistry,
) -> Vec<(String, ScheduledTask)> {
    let mut all_tasks = Vec::new();

    // Root project schedule
    let root_schedule_path = workspace_root.join("schedule.json");
    if let Ok(schedule) = load_schedule_file(&root_schedule_path) {
        for task in schedule.tasks {
            all_tasks.push(("root".to_string(), task));
        }
    }

    // Per-project schedules
    for project in projects.all() {
        let schedule_path = project.workspace_path.join("schedule.json");
        if let Ok(schedule) = load_schedule_file(&schedule_path) {
            for task in schedule.tasks {
                all_tasks.push((project.name.clone(), task));
            }
        }
    }

    all_tasks
}
```
