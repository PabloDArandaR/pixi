#![allow(dead_code)]

pub mod builders;
pub mod client;
pub mod package_database;

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Output,
    str::FromStr,
};

use builders::{LockBuilder, SearchBuilder};
use indicatif::ProgressDrawTarget;
use miette::{Context, Diagnostic, IntoDiagnostic};
use pixi::{
    UpdateLockFileOptions, Workspace,
    cli::{
        LockFileUsageConfig, add,
        cli_config::{ChannelsConfig, LockFileUpdateConfig, PrefixUpdateConfig, WorkspaceConfig},
        init::{self, GitAttributes},
        install::Args,
        lock, remove, run, search,
        task::{self, AddArgs, AliasArgs},
        update, workspace,
    },
    lock_file::{ReinstallPackages, UpdateMode},
    task::{
        ExecutableTask, RunOutput, SearchEnvironments, TaskExecutionError, TaskGraph,
        TaskGraphError, TaskName, get_task_env,
    },
};
use pixi_consts::consts;
use pixi_manifest::{EnvironmentName, FeatureName};
use pixi_progress::global_multi_progress;
use rattler_conda_types::{MatchSpec, ParseStrictness::Lenient, Platform};
use rattler_lock::{LockFile, LockedPackageRef, UrlOrPath};
use tempfile::TempDir;
use thiserror::Error;

use self::builders::{HasDependencyConfig, RemoveBuilder};
use crate::common::builders::{
    AddBuilder, InitBuilder, InstallBuilder, ProjectChannelAddBuilder, ProjectChannelRemoveBuilder,
    ProjectEnvironmentAddBuilder, TaskAddBuilder, TaskAliasBuilder, UpdateBuilder,
};

const DEFAULT_PROJECT_CONFIG: &str = r#"
[repodata-config."https://prefix.dev"]
disable-sharded = false
"#;

/// To control the pixi process
pub struct PixiControl {
    /// The path to the project working file
    tmpdir: TempDir,
}

pub struct RunResult {
    output: Output,
}

/// Hides the progress bars for the tests
fn hide_progress_bars() {
    global_multi_progress().set_draw_target(ProgressDrawTarget::hidden());
}

impl RunResult {
    /// Was the output successful
    pub fn success(&self) -> bool {
        self.output.status.success()
    }

    /// Get the output
    pub fn stdout(&self) -> &str {
        std::str::from_utf8(&self.output.stdout).expect("could not get output")
    }
}

/// MatchSpecs from an iterator
pub fn string_from_iter(iter: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<String> {
    iter.into_iter().map(|s| s.as_ref().to_string()).collect()
}

pub trait LockFileExt {
    /// Check if this package is contained in the lockfile
    fn contains_conda_package(&self, environment: &str, platform: Platform, name: &str) -> bool;
    fn contains_pypi_package(&self, environment: &str, platform: Platform, name: &str) -> bool;
    /// Check if this matchspec is contained in the lockfile
    fn contains_match_spec(
        &self,
        environment: &str,
        platform: Platform,
        match_spec: impl IntoMatchSpec,
    ) -> bool;

    /// Check if the pep508 requirement is contained in the lockfile for this
    /// platform
    fn contains_pep508_requirement(
        &self,
        environment: &str,
        platform: Platform,
        requirement: pep508_rs::Requirement,
    ) -> bool;

    fn get_pypi_package_version(
        &self,
        environment: &str,
        platform: Platform,
        package: &str,
    ) -> Option<String>;

    fn get_pypi_package_url(
        &self,
        environment: &str,
        platform: Platform,
        package: &str,
    ) -> Option<UrlOrPath>;
}

impl LockFileExt for LockFile {
    fn contains_conda_package(&self, environment: &str, platform: Platform, name: &str) -> bool {
        let Some(env) = self.environment(environment) else {
            return false;
        };
        let package_found = env
            .packages(platform)
            .into_iter()
            .flatten()
            .filter_map(LockedPackageRef::as_conda)
            .any(|package| package.record().name.as_normalized() == name);
        package_found
    }
    fn contains_pypi_package(&self, environment: &str, platform: Platform, name: &str) -> bool {
        let Some(env) = self.environment(environment) else {
            return false;
        };
        let package_found = env
            .packages(platform)
            .into_iter()
            .flatten()
            .filter_map(LockedPackageRef::as_pypi)
            .any(|(data, _)| data.name.as_ref() == name);
        package_found
    }

    fn contains_match_spec(
        &self,
        environment: &str,
        platform: Platform,
        match_spec: impl IntoMatchSpec,
    ) -> bool {
        let match_spec = match_spec.into();
        let Some(env) = self.environment(environment) else {
            return false;
        };
        let package_found = env
            .packages(platform)
            .into_iter()
            .flatten()
            .filter_map(LockedPackageRef::as_conda)
            .any(move |p| p.satisfies(&match_spec));
        package_found
    }

    fn contains_pep508_requirement(
        &self,
        environment: &str,
        platform: Platform,
        requirement: pep508_rs::Requirement,
    ) -> bool {
        let Some(env) = self.environment(environment) else {
            eprintln!("environment not found: {}", environment);
            return false;
        };
        let package_found = env
            .packages(platform)
            .into_iter()
            .flatten()
            .filter_map(LockedPackageRef::as_pypi)
            .any(move |(data, _)| data.satisfies(&requirement));
        package_found
    }

    fn get_pypi_package_version(
        &self,
        environment: &str,
        platform: Platform,
        package: &str,
    ) -> Option<String> {
        self.environment(environment)
            .and_then(|env| {
                env.pypi_packages(platform).and_then(|mut packages| {
                    packages.find(|(data, _)| data.name.as_ref() == package)
                })
            })
            .map(|(data, _)| data.version.to_string())
    }

    fn get_pypi_package_url(
        &self,
        environment: &str,
        platform: Platform,
        package: &str,
    ) -> Option<UrlOrPath> {
        self.environment(environment)
            .and_then(|env| {
                env.packages(platform)
                    .and_then(|mut packages| packages.find(|p| p.name() == package))
            })
            .map(|p| p.location().clone())
    }
}

impl PixiControl {
    /// Create a new PixiControl instance
    pub fn new() -> miette::Result<PixiControl> {
        let tempdir = tempfile::tempdir().into_diagnostic()?;

        // Add default project config
        let pixi_path = tempdir.path().join(".pixi");
        fs_err::create_dir_all(&pixi_path).unwrap();
        fs_err::write(pixi_path.join("config.toml"), DEFAULT_PROJECT_CONFIG).unwrap();

        // Hide the progress bars for the tests
        // Otherwise the override the test output
        hide_progress_bars();
        Ok(PixiControl { tmpdir: tempdir })
    }

    /// Creates a new PixiControl instance from an existing manifest
    pub fn from_manifest(manifest: &str) -> miette::Result<PixiControl> {
        let pixi = Self::new()?;
        fs_err::write(pixi.manifest_path(), manifest)
            .into_diagnostic()
            .context("failed to write pixi.toml")?;
        Ok(pixi)
    }

    /// Creates a new PixiControl instance from an pyproject manifest
    pub fn from_pyproject_manifest(pyproject_manifest: &str) -> miette::Result<PixiControl> {
        let pixi = Self::new()?;
        fs_err::write(pixi.pyproject_manifest_path(), pyproject_manifest)
            .into_diagnostic()
            .context("failed to write pixi.toml")?;
        Ok(pixi)
    }

    /// Updates the complete manifest
    pub fn update_manifest(&self, manifest: &str) -> miette::Result<()> {
        fs_err::write(self.manifest_path(), manifest)
            .into_diagnostic()
            .context("failed to write pixi.toml")?;
        Ok(())
    }

    /// Loads the workspace manifest and returns it.
    pub fn workspace(&self) -> miette::Result<Workspace> {
        Workspace::from_path(&self.manifest_path()).into_diagnostic()
    }

    /// Get the path to the workspace
    pub fn workspace_path(&self) -> &Path {
        self.tmpdir.path()
    }

    /// Get path to default environment
    pub fn default_env_path(&self) -> miette::Result<PathBuf> {
        let project = self.workspace()?;
        let env = project.environment("default");
        let env = env.ok_or_else(|| miette::miette!("default environment not found"))?;
        Ok(self.tmpdir.path().join(env.dir()))
    }

    /// Get path to default environment
    pub fn env_path(&self, env_name: &str) -> miette::Result<PathBuf> {
        let workspace = self.workspace()?;
        let env = workspace.environment(env_name);
        let env = env.ok_or_else(|| miette::miette!("{} environment not found", env_name))?;
        Ok(self.tmpdir.path().join(env.dir()))
    }

    pub fn manifest_path(&self) -> PathBuf {
        // Either pixi.toml or pyproject.toml
        if self
            .workspace_path()
            .join(consts::WORKSPACE_MANIFEST)
            .exists()
        {
            self.workspace_path().join(consts::WORKSPACE_MANIFEST)
        } else if self
            .workspace_path()
            .join(consts::PYPROJECT_MANIFEST)
            .exists()
        {
            self.workspace_path().join(consts::PYPROJECT_MANIFEST)
        } else {
            self.workspace_path().join(consts::WORKSPACE_MANIFEST)
        }
    }

    pub(crate) fn pyproject_manifest_path(&self) -> PathBuf {
        self.workspace_path().join(consts::PYPROJECT_MANIFEST)
    }

    /// Get the manifest contents
    pub fn manifest_contents(&self) -> miette::Result<String> {
        fs_err::read_to_string(self.manifest_path())
            .into_diagnostic()
            .context("failed to read manifest")
    }

    /// Initialize pixi project inside a temporary directory. Returns a
    /// [`InitBuilder`]. To execute the command and await the result call
    /// `.await` on the return value.
    pub fn init(&self) -> InitBuilder {
        InitBuilder {
            no_fast_prefix: false,
            args: init::Args {
                path: self.workspace_path().to_path_buf(),
                channels: None,
                platforms: Vec::new(),
                env_file: None,
                format: None,
                pyproject_toml: false,
                scm: Some(GitAttributes::Github),
            },
        }
    }

    /// Initialize pixi project inside a temporary directory. Returns a
    /// [`InitBuilder`]. To execute the command and await the result, call
    /// `.await` on the return value.
    pub fn init_with_platforms(&self, platforms: Vec<String>) -> InitBuilder {
        InitBuilder {
            no_fast_prefix: false,
            args: init::Args {
                path: self.workspace_path().to_path_buf(),
                channels: None,
                platforms,
                env_file: None,
                format: None,
                pyproject_toml: false,
                scm: Some(GitAttributes::Github),
            },
        }
    }

    /// Add a dependency to the project. Returns an [`AddBuilder`].
    /// To execute the command and await the result, call `.await` on the return value.
    pub fn add(&self, spec: &str) -> AddBuilder {
        self.add_multiple(vec![spec])
    }

    /// Add dependencies to the project. Returns an [`AddBuilder`].
    /// To execute the command and await the result, call `.await` on the return value.
    pub fn add_multiple(&self, specs: Vec<&str>) -> AddBuilder {
        AddBuilder {
            args: add::Args {
                workspace_config: WorkspaceConfig {
                    manifest_path: Some(self.manifest_path()),
                },
                dependency_config: AddBuilder::dependency_config_with_specs(specs),
                prefix_update_config: PrefixUpdateConfig {
                    no_install: true,

                    revalidate: false,
                },
                lock_file_update_config: LockFileUpdateConfig {
                    no_lockfile_update: false,
                    lock_file_usage: LockFileUsageConfig::default(),
                },
                config: Default::default(),
                editable: false,
            },
        }
    }

    /// Search and return latest package. Returns an [`SearchBuilder`].
    /// the command and await the result call `.await` on the return value.
    pub fn search(&self, name: String) -> SearchBuilder {
        SearchBuilder {
            args: search::Args {
                package: name,
                project_config: WorkspaceConfig {
                    manifest_path: Some(self.manifest_path()),
                },
                platform: Platform::current(),
                limit: None,
                channels: ChannelsConfig::default(),
            },
        }
    }

    /// Remove dependencies from the project. Returns a [`RemoveBuilder`].
    pub fn remove(&self, spec: &str) -> RemoveBuilder {
        RemoveBuilder {
            args: remove::Args {
                workspace_config: WorkspaceConfig {
                    manifest_path: Some(self.manifest_path()),
                },
                dependency_config: AddBuilder::dependency_config_with_specs(vec![spec]),
                prefix_update_config: PrefixUpdateConfig {
                    no_install: true,
                    revalidate: false,
                },
                lock_file_update_config: LockFileUpdateConfig {
                    no_lockfile_update: false,
                    lock_file_usage: LockFileUsageConfig::default(),
                },
                config: Default::default(),
            },
        }
    }

    /// Add a new channel to the project.
    pub fn project_channel_add(&self) -> ProjectChannelAddBuilder {
        ProjectChannelAddBuilder {
            args: workspace::channel::AddRemoveArgs {
                workspace_config: WorkspaceConfig {
                    manifest_path: Some(self.manifest_path()),
                },
                channel: vec![],
                prefix_update_config: PrefixUpdateConfig {
                    no_install: true,
                    revalidate: false,
                },
                lock_file_update_config: LockFileUpdateConfig {
                    no_lockfile_update: false,
                    lock_file_usage: LockFileUsageConfig::default(),
                },
                config: Default::default(),
                feature: None,
                priority: None,
                prepend: false,
            },
        }
    }

    /// Add a new channel to the project.
    pub fn project_channel_remove(&self) -> ProjectChannelRemoveBuilder {
        ProjectChannelRemoveBuilder {
            manifest_path: Some(self.manifest_path()),
            args: workspace::channel::AddRemoveArgs {
                workspace_config: WorkspaceConfig {
                    manifest_path: Some(self.manifest_path()),
                },
                channel: vec![],
                prefix_update_config: PrefixUpdateConfig {
                    no_install: true,
                    revalidate: false,
                },
                lock_file_update_config: LockFileUpdateConfig {
                    no_lockfile_update: false,
                    lock_file_usage: LockFileUsageConfig::default(),
                },
                config: Default::default(),
                feature: None,
                priority: None,
                prepend: false,
            },
        }
    }

    pub fn project_environment_add(&self, name: EnvironmentName) -> ProjectEnvironmentAddBuilder {
        ProjectEnvironmentAddBuilder {
            manifest_path: Some(self.manifest_path()),
            args: workspace::environment::add::Args {
                name,
                features: None,
                solve_group: None,
                no_default_feature: false,
                force: false,
            },
        }
    }

    /// Run a command
    pub async fn run(&self, mut args: run::Args) -> miette::Result<RunOutput> {
        args.workspace_config.manifest_path = args
            .workspace_config
            .manifest_path
            .or_else(|| Some(self.manifest_path()));

        // Load the project
        let project = self.workspace()?;

        // Extract the passed in environment name.
        let explicit_environment = args
            .environment
            .map(|n| EnvironmentName::from_str(n.as_str()))
            .transpose()?
            .map(|n| {
                project
                    .environment(&n)
                    .ok_or_else(|| miette::miette!("unknown environment '{n}'"))
            })
            .transpose()?;

        // Ensure the lock-file is up-to-date
        let lock_file = project
            .update_lock_file(UpdateLockFileOptions {
                lock_file_usage: args.lock_file_update_config.lock_file_usage()?,
                ..UpdateLockFileOptions::default()
            })
            .await?;

        // Create a task graph from the command line arguments.
        let search_env = SearchEnvironments::from_opt_env(
            &project,
            explicit_environment.clone(),
            explicit_environment
                .as_ref()
                .map(|e| e.best_platform())
                .or(Some(Platform::current())),
        );
        let task_graph = TaskGraph::from_cmd_args(&project, &search_env, args.task, false)
            .map_err(RunError::TaskGraphError)?;

        // Iterate over all tasks in the graph and execute them.
        let mut task_env = None;
        let mut result = RunOutput::default();
        for task_id in task_graph.topological_order() {
            let task = ExecutableTask::from_task_graph(&task_graph, task_id);

            // Construct the task environment if not already created.
            let task_env = match task_env.as_ref() {
                None => {
                    lock_file
                        .prefix(
                            &task.run_environment,
                            UpdateMode::Revalidate,
                            &ReinstallPackages::default(),
                        )
                        .await?;
                    let env =
                        get_task_env(&task.run_environment, args.clean_env, None, false, false)
                            .await?;
                    task_env.insert(env)
                }
                Some(task_env) => task_env,
            };

            let task_env = task_env
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                .collect();

            let output = task.execute_with_pipes(&task_env, None).await?;
            result.stdout.push_str(&output.stdout);
            result.stderr.push_str(&output.stderr);
            result.exit_code = output.exit_code;
            if output.exit_code != 0 {
                return Err(RunError::NonZeroExitCode(output.exit_code).into());
            }
        }

        Ok(result)
    }

    /// Returns a [`InstallBuilder`]. To execute the command and await the
    /// result call `.await` on the return value.
    pub fn install(&self) -> InstallBuilder {
        InstallBuilder {
            args: Args {
                environment: None,
                project_config: WorkspaceConfig {
                    manifest_path: Some(self.manifest_path()),
                },
                lock_file_usage: LockFileUsageConfig {
                    frozen: false,
                    locked: false,
                },
                config: Default::default(),
                all: false,
            },
        }
    }

    /// Returns a [`UpdateBuilder]. To execute the command and await the result
    /// call `.await` on the return value.
    pub fn update(&self) -> UpdateBuilder {
        UpdateBuilder {
            args: update::Args {
                config: Default::default(),
                project_config: WorkspaceConfig {
                    manifest_path: Some(self.manifest_path()),
                },
                no_install: true,
                dry_run: false,
                specs: Default::default(),
                json: false,
            },
        }
    }

    /// Load the current lock-file.
    ///
    /// If you want to lock-file to be up-to-date with the project call
    /// [`Self::update_lock_file`].
    pub async fn lock_file(&self) -> miette::Result<LockFile> {
        let workspace = Workspace::from_path(&self.manifest_path())?;
        workspace.load_lock_file().await
    }

    /// Load the current lock-file and makes sure that its up to date with the
    /// project.
    pub async fn update_lock_file(&self) -> miette::Result<LockFile> {
        let project = self.workspace()?;
        Ok(project
            .update_lock_file(UpdateLockFileOptions::default())
            .await?
            .into_lock_file())
    }

    /// Returns an [`LockBuilder`].
    /// To execute the command and await the result, call `.await` on the return value.
    pub fn lock(&self) -> LockBuilder {
        LockBuilder {
            args: lock::Args {
                workspace_config: WorkspaceConfig {
                    manifest_path: Some(self.manifest_path()),
                },
                check: false,
                json: false,
            },
        }
    }

    pub fn tasks(&self) -> TasksControl {
        TasksControl { pixi: self }
    }
}

pub struct TasksControl<'a> {
    /// Reference to the pixi control
    pixi: &'a PixiControl,
}

impl TasksControl<'_> {
    /// Add a task
    pub fn add(
        &self,
        name: TaskName,
        platform: Option<Platform>,
        feature_name: FeatureName,
    ) -> TaskAddBuilder {
        TaskAddBuilder {
            manifest_path: Some(self.pixi.manifest_path()),
            args: AddArgs {
                name,
                commands: vec![],
                depends_on: None,
                platform,
                feature: feature_name.non_default().map(str::to_owned),
                cwd: None,
                env: Default::default(),
                description: None,
                clean_env: false,
                args: None,
            },
        }
    }

    /// Remove a task
    pub async fn remove(
        &self,
        name: TaskName,
        platform: Option<Platform>,
        feature_name: Option<String>,
    ) -> miette::Result<()> {
        task::execute(task::Args {
            workspace_config: WorkspaceConfig {
                manifest_path: Some(self.pixi.manifest_path()),
            },
            operation: task::Operation::Remove(task::RemoveArgs {
                names: vec![name],
                platform,
                feature: feature_name,
            }),
        })
        .await
    }

    /// Alias one or multiple tasks
    pub fn alias(&self, name: TaskName, platform: Option<Platform>) -> TaskAliasBuilder {
        TaskAliasBuilder {
            manifest_path: Some(self.pixi.manifest_path()),
            args: AliasArgs {
                platform,
                alias: name,
                depends_on: vec![],
                description: None,
            },
        }
    }
}

/// A helper trait to convert from different types into a [`MatchSpec`] to make
/// it simpler to use them in tests.
pub trait IntoMatchSpec {
    fn into(self) -> MatchSpec;
}

impl IntoMatchSpec for &str {
    fn into(self) -> MatchSpec {
        MatchSpec::from_str(self, Lenient).unwrap()
    }
}

impl IntoMatchSpec for String {
    fn into(self) -> MatchSpec {
        MatchSpec::from_str(&self, Lenient).unwrap()
    }
}

impl IntoMatchSpec for MatchSpec {
    fn into(self) -> MatchSpec {
        self
    }
}

#[derive(Error, Debug, Diagnostic)]
enum RunError {
    #[error(transparent)]
    TaskGraphError(#[from] TaskGraphError),
    #[error(transparent)]
    ExecutionError(#[from] TaskExecutionError),
    #[error("the task executed with a non-zero exit code {0}")]
    NonZeroExitCode(i32),
}
