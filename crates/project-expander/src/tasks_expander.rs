use crate::expander_context::*;
use crate::tasks_expander_error::TasksExpanderError;
use crate::token_expander::TokenExpander;
use moon_common::color;
use moon_config::{TaskArgs, TaskDependencyConfig};
use moon_project::Project;
use moon_task::{Target, TargetScope, Task};
use moon_task_args::parse_task_args;
use rustc_hash::FxHashMap;
use tracing::{instrument, trace, warn};

pub struct TasksExpander<'graph, 'query> {
    pub context: &'graph ExpanderContext<'graph, 'query>,
    pub token: TokenExpander<'graph, 'query>,
}

impl<'graph, 'query> TasksExpander<'graph, 'query> {
    pub fn new(context: &'graph ExpanderContext<'graph, 'query>) -> Self {
        Self {
            token: TokenExpander::new(context),
            context,
        }
    }

    #[instrument(skip_all)]
    pub fn expand_command(&mut self, task: &mut Task) -> miette::Result<()> {
        trace!(
            target = task.target.as_str(),
            command = &task.command,
            "Expanding tokens and variables in command"
        );

        // Token variables
        let command = self.token.expand_command(task)?;

        // Environment variables
        let command = substitute_env_var("", &command, &task.env);

        task.command = command;

        Ok(())
    }

    #[instrument(skip_all)]
    pub fn expand_script(&mut self, task: &mut Task) -> miette::Result<()> {
        trace!(
            target = task.target.as_str(),
            script = task.script.as_ref(),
            "Expanding tokens and variables in script"
        );

        // Token variables
        let script = self.token.expand_script(task)?;

        // Environment variables
        let script = substitute_env_var("", &script, &task.env);

        task.script = Some(script);

        Ok(())
    }

    #[instrument(skip_all)]
    pub fn expand_args(&mut self, task: &mut Task) -> miette::Result<()> {
        if task.args.is_empty() {
            return Ok(());
        }

        trace!(
            target = task.target.as_str(),
            args = ?task.args,
            "Expanding tokens and variables in args",
        );

        task.args = self.token.expand_args(task)?;

        Ok(())
    }

    #[instrument(skip_all)]
    pub fn expand_deps(&mut self, task: &mut Task) -> miette::Result<()> {
        if task.deps.is_empty() {
            return Ok(());
        }

        trace!(
            target = task.target.as_str(),
            deps = ?task.deps.iter().map(|d| d.target.as_str()).collect::<Vec<_>>(),
            "Expanding target scopes for deps",
        );

        let project = &self.context.project;

        // Dont use a `HashSet` as we want to preserve order
        let mut deps: Vec<TaskDependencyConfig> = vec![];

        let mut check_and_push_dep = |dep_project: &Project,
                                      dep: &TaskDependencyConfig,
                                      skip_if_missing: bool|
         -> miette::Result<()> {
            // Allow internal tasks!
            let Some(dep_task) = dep_project.tasks.get(&dep.target.task_id) else {
                if skip_if_missing {
                    return Ok(());
                }

                return Err(TasksExpanderError::UnknownTarget {
                    dep: Target::new(&dep_project.id, &dep.target.task_id)?,
                    task: task.target.to_owned(),
                }
                .into());
            };

            // Do not depend on tasks that can fail
            if dep_task.options.allow_failure {
                return Err(TasksExpanderError::AllowFailureDepRequirement {
                    dep: dep_task.target.to_owned(),
                    task: task.target.to_owned(),
                }
                .into());
            }

            // Do not depend on tasks that can't run in CI
            if self.context.check_ci_relationships
                && !dep_task.options.run_in_ci
                && task.options.run_in_ci
            {
                return Err(TasksExpanderError::RunInCiDepRequirement {
                    dep: dep_task.target.to_owned(),
                    task: task.target.to_owned(),
                }
                .into());
            }

            // Enforce persistent constraints
            if dep_task.is_persistent() && !task.is_persistent() {
                return Err(TasksExpanderError::PersistentDepRequirement {
                    dep: dep_task.target.to_owned(),
                    task: task.target.to_owned(),
                }
                .into());
            }

            // Add the dep if it has not already been
            let mut dep_args = parse_task_args(&dep.args)?;
            let dep_env = self.token.expand_env_with_task(task, &dep.env)?;

            if !dep_args.is_empty() {
                dep_args = self.token.expand_args_with_task(task, &dep_args)?;
            }

            let dep = TaskDependencyConfig {
                args: if dep_args.is_empty() {
                    TaskArgs::None
                } else {
                    TaskArgs::List(dep_args)
                },
                env: substitute_env_vars(dep_env),
                optional: dep.optional,
                target: Target::new(&dep_project.id, &dep.target.task_id)?,
            };

            if !deps.contains(&dep) {
                deps.push(dep);
            }

            Ok(())
        };

        for dep in &task.deps {
            let dep_target = &dep.target;

            match &dep_target.scope {
                // :task
                TargetScope::All => {
                    return Err(TasksExpanderError::UnsupportedTargetScopeInDeps {
                        dep: dep_target.to_owned(),
                        task: task.target.to_owned(),
                    }
                    .into());
                }
                // ^:task
                TargetScope::Deps => {
                    let mut dep_ids = project
                        .get_dependency_ids()
                        .iter()
                        .map(|id| id.to_string())
                        .collect::<Vec<_>>();

                    if !dep_ids.is_empty() {
                        // Sort so query cache is more deterministic
                        dep_ids.sort();

                        let input = if dep_ids.len() == 1 {
                            format!("project={id}", id = dep_ids[0])
                        } else {
                            format!("project=[{ids}]", ids = dep_ids.join(","))
                        };

                        for dep_project in (self.context.query)(input)? {
                            check_and_push_dep(dep_project, dep, dep.optional.unwrap_or(true))?;
                        }
                    }
                }
                // ~:task
                TargetScope::OwnSelf => {
                    if dep_target.task_id == task.id {
                        // Avoid circular references
                    } else {
                        check_and_push_dep(project, dep, dep.optional.unwrap_or(false))?;
                    }
                }
                // id:task
                TargetScope::Project(project_locator) => {
                    if project.matches_locator(project_locator) {
                        if dep_target.task_id == task.id {
                            // Avoid circular references
                        } else {
                            check_and_push_dep(project, dep, false)?;
                        }
                    } else {
                        let results = (self.context.query)(format!("project={}", project_locator))?;

                        if results.is_empty() {
                            return Err(TasksExpanderError::UnknownTarget {
                                dep: dep_target.to_owned(),
                                task: task.target.to_owned(),
                            }
                            .into());
                        }

                        for dep_project in results {
                            check_and_push_dep(dep_project, dep, false)?;
                        }
                    }
                }
                // #tag:task
                TargetScope::Tag(tag) => {
                    for dep_project in (self.context.query)(format!("tag={tag}"))? {
                        if dep_project.id == project.id {
                            // Avoid circular references
                        } else {
                            check_and_push_dep(dep_project, dep, dep.optional.unwrap_or(true))?;
                        }
                    }
                }
            }
        }

        task.deps = deps;

        Ok(())
    }

    #[instrument(skip_all)]
    pub fn expand_env(&mut self, task: &mut Task) -> miette::Result<()> {
        trace!(
            target = task.target.as_str(),
            env = ?task.env,
            "Expanding environment variables"
        );

        let mut env = self.token.expand_env(task)?;

        // Load variables from an .env file
        if let Some(env_files) = &task.options.env_files {
            let env_paths = env_files
                .iter()
                .map(|file| {
                    file.to_workspace_relative(self.context.project.source.as_str())
                        .to_path(self.context.workspace_root)
                })
                .collect::<Vec<_>>();

            trace!(
                target = task.target.as_str(),
                env_files = ?env_paths,
                "Loading environment variables from .env files",
            );

            let mut missing_paths = vec![];
            let mut merged_env_vars = FxHashMap::default();

            // The file may not have been committed, so avoid crashing
            for env_path in env_paths {
                if env_path.exists() {
                    let handle_error = |error: dotenvy::Error| TasksExpanderError::InvalidEnvFile {
                        path: env_path.to_path_buf(),
                        error: Box::new(error),
                    };

                    for line in dotenvy::from_path_iter(&env_path).map_err(handle_error)? {
                        let (key, val) = line.map_err(handle_error)?;

                        // Overwrite previous values
                        merged_env_vars.insert(key, val);
                    }
                } else {
                    missing_paths.push(env_path);
                }
            }

            // Don't override task-level variables
            for (key, val) in merged_env_vars {
                env.entry(key).or_insert(val);
            }

            if !missing_paths.is_empty() {
                warn!(
                    target = task.target.as_str(),
                    env_files = ?missing_paths,
                    "Setting {} is enabled but file(s) don't exist, skipping as this may be intentional",
                    color::property("options.envFile"),
                );
            }
        }

        task.env = substitute_env_vars(env);

        Ok(())
    }

    #[instrument(skip_all)]
    pub fn expand_inputs(&mut self, task: &mut Task) -> miette::Result<()> {
        if task.inputs.is_empty() {
            return Ok(());
        }

        trace!(
            target = task.target.as_str(),
            inputs = ?task.inputs.iter().map(|d| d.as_str()).collect::<Vec<_>>(),
            "Expanding inputs into file system paths"
        );

        // Expand inputs to file system paths and environment variables
        let result = self.token.expand_inputs(task)?;

        task.input_env.extend(result.env);
        task.input_files.extend(result.files);
        task.input_globs.extend(result.globs);

        Ok(())
    }

    #[instrument(skip_all)]
    pub fn expand_outputs(&mut self, task: &mut Task) -> miette::Result<()> {
        if task.outputs.is_empty() {
            return Ok(());
        }

        trace!(
            target = task.target.as_str(),
            outputs = ?task.outputs.iter().map(|d| d.as_str()).collect::<Vec<_>>(),
            "Expanding outputs into file system paths"
        );

        // Expand outputs to file system paths
        let result = self.token.expand_outputs(task)?;

        // Aggregate paths first before globbing, as they are literal
        for file in result.files {
            // Outputs must *not* be considered an input,
            // so if there's an input that matches an output,
            // remove it! Is there a better way to do this?
            if task.input_files.contains(&file) {
                task.input_files.remove(&file);
            }

            task.output_files.insert(file);
        }

        // Aggregate globs second so we can match against the paths
        for glob in result.globs {
            if task.input_globs.contains(&glob) {
                task.input_globs.remove(&glob);
            }

            task.output_globs.insert(glob);
        }

        Ok(())
    }
}
