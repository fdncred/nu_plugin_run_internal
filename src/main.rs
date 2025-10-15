use nu_cli::gather_parent_env_vars;
use nu_engine::{convert_env_values, eval_block};
use nu_parser::parse;
use nu_plugin::{
    EngineInterface, EvaluatedCall, MsgPackSerializer, Plugin, PluginCommand, serve_plugin,
};
use nu_protocol::{
    Category, Example, IntoValue, LabeledError, PipelineData, ShellError, Signature, Spanned,
    SyntaxShape, Type, Value, debugger::WithoutDebug, engine::EngineState, engine::Stack,
    engine::StateWorkingSet, report_error::report_compile_error, report_parse_error,
    report_parse_warning, report_shell_error,
};
use std::{path::PathBuf, sync::Arc};

pub struct RunInternalPlugin;

impl Plugin for RunInternalPlugin {
    fn version(&self) -> String {
        // This automatically uses the version of your package from Cargo.toml as the plugin version
        // sent to Nushell
        env!("CARGO_PKG_VERSION").into()
    }

    fn commands(&self) -> Vec<Box<dyn PluginCommand<Plugin = Self>>> {
        vec![
            // Commands should be added here
            Box::new(RunInternal),
        ]
    }
}

pub struct RunInternal;

impl PluginCommand for RunInternal {
    type Plugin = RunInternalPlugin;

    fn name(&self) -> &str {
        "run-internal"
    }

    fn signature(&self) -> Signature {
        Signature::build(self.name())
            .input_output_types(vec![(Type::Any, Type::Any)])
            .required("command", SyntaxShape::String, "Internal command to run.")
            .category(Category::Experimental)
    }

    fn description(&self) -> &str {
        "Runs internal command."
    }

    fn examples(&self) -> Vec<Example<'_>> {
        vec![
            Example {
                description: "Run an internal command",
                example: r#"run-internal "ls""#,
                result: None,
            },
            Example {
                description: "Run a pipeline",
                example: r#"run-internal "print (ls | first 5);print (ps | first 5)"#,
                result: None,
            },
        ]
    }

    fn run(
        &self,
        _plugin: &RunInternalPlugin,
        engine_interface: &EngineInterface,
        call: &EvaluatedCall,
        input: PipelineData,
    ) -> Result<PipelineData, LabeledError> {
        let config = engine_interface.get_config()?;
        let table_mode = config.table.mode.into_value(call.head);
        let error_style = config.error_style.into_value(call.head);
        let commands: Spanned<String> = call.req(0)?;

        let mut stack = Stack::new();
        // let engine_state = create_default_context();
        // let engine_state = add_shell_command_context(engine_state);
        // let mut engine_state = add_extra_command_context(engine_state);
        let engine_state = nu_cmd_lang::create_default_context();
        let engine_state = nu_command::add_shell_command_context(engine_state);
        let engine_state = nu_cmd_extra::add_extra_command_context(engine_state);
        let mut engine_state = nu_cli::add_cli_context(engine_state);

        let curdir = engine_interface.get_current_dir()?;
        // let _ = stack.set_cwd(curdir);
        if let Err(err) = stack.set_cwd(curdir) {
            report_shell_error(&engine_state, &err);
        };

        engine_state.cwd(Some(&stack))?;

        // Get the current working directory from the environment.
        let init_cwd = current_dir_from_environment();

        // Custom additions
        let delta = {
            let mut working_set = StateWorkingSet::new(&engine_state);
            working_set.add_decl(Box::new(nu_cli::NuHighlight));
            working_set.add_decl(Box::new(nu_cli::Print));
            working_set.render()
        };

        if let Err(err) = engine_state.merge_delta(delta) {
            report_shell_error(&engine_state, &err);
        }

        // First, set up env vars as strings only
        gather_parent_env_vars(&mut engine_state, init_cwd.as_ref());

        evaluate_commands(
            &commands,
            &mut engine_state.clone(),
            &mut engine_interface.clone(),
            &mut stack.clone(),
            input,
            EvaluateCommandsOpts {
                table_mode: Some(table_mode),
                error_style: Some(error_style),
            },
        )
    }
}

/// Get the directory where the Nushell executable is located.
fn current_exe_directory() -> PathBuf {
    let mut path = std::env::current_exe().expect("current_exe() should succeed");
    path.pop();
    path
}

/// Get the current working directory from the environment.
fn current_dir_from_environment() -> PathBuf {
    if let Ok(cwd) = std::env::current_dir() {
        return cwd;
    }
    if let Ok(cwd) = std::env::var("PWD") {
        return cwd.into();
    }
    if let Some(home) = nu_path::home_dir() {
        return home.into_std_path_buf();
    }
    current_exe_directory()
}

// This code is ripped off from nu-cli. It's duplicated here because I didn't
// want to add a dependency on nu-cli in nu-command.
#[derive(Default)]
pub struct EvaluateCommandsOpts {
    pub table_mode: Option<Value>,
    pub error_style: Option<Value>,
}

/// Run a command (or commands) given to us by the user
pub fn evaluate_commands(
    commands: &Spanned<String>,
    engine_state: &mut EngineState,
    engine_interface: &mut EngineInterface,
    stack: &mut Stack,
    input: PipelineData,
    opts: EvaluateCommandsOpts,
) -> Result<PipelineData, LabeledError> {
    let EvaluateCommandsOpts {
        table_mode,
        error_style,
    } = opts;

    // Handle the configured error style early
    if let Some(e_style) = error_style {
        match e_style.coerce_str()?.parse() {
            Ok(e_style) => {
                Arc::make_mut(&mut engine_interface.get_config()?).error_style = e_style;
            }
            Err(err) => {
                return Err(ShellError::GenericError {
                    error: "Invalid value for `--error-style`".into(),
                    msg: err.into(),
                    span: Some(e_style.span()),
                    help: None,
                    inner: vec![],
                })
                .map_err(|e| e.into());
            }
        }
    }

    // Translate environment variables from Strings to Values
    convert_env_values(engine_state, stack)?;

    // Parse the source code
    let (block, delta) = {
        if let Some(ref t_mode) = table_mode {
            Arc::make_mut(&mut engine_interface.get_config()?)
                .table
                .mode = t_mode.coerce_str()?.parse().unwrap_or_default();
        }

        let mut working_set = StateWorkingSet::new(engine_state);

        let output = parse(&mut working_set, None, commands.item.as_bytes(), false);
        if let Some(warning) = working_set.parse_warnings.first() {
            report_parse_warning(&working_set, warning);
        }

        if let Some(err) = working_set.parse_errors.first() {
            report_parse_error(&working_set, err);
        }

        if let Some(err) = working_set.compile_errors.first() {
            report_compile_error(&working_set, err);
            // Not a fatal error, for now
        }

        (output, working_set.render())
    };

    // Update permanent state
    engine_state.merge_delta(delta)?;

    // Run the block
    // let pipeline = eval_block::<WithoutDebug>(engine_state, stack, &block, input)?;
    let pipeline = eval_block::<WithoutDebug>(engine_state, stack, &block, input)?;
    let pipeline_data = pipeline.body;
    if let PipelineData::Value(Value::Error { error, .. }, ..) = pipeline_data {
        return Err((*error).into());
    } else {
        Ok(pipeline_data)
    }
    // if let PipelineData::Value(Value::Error { error, .. }, ..) = pipeline {
    //     return Err(*error);
    // }

    // if let Some(t_mode) = table_mode {
    //     Arc::make_mut(&mut engine_state.config).table.mode =
    //         t_mode.coerce_str()?.parse().unwrap_or_default();
    // }

    // pipeline.print(engine_state, stack, no_newline, false)?;

    // info!("evaluate {}:{}:{}", file!(), line!(), column!());

    // Ok(())
}

#[test]
fn test_examples() -> Result<(), nu_protocol::ShellError> {
    use nu_plugin_test_support::PluginTest;

    // This will automatically run the examples specified in your command and compare their actual
    // output against what was specified in the example. You can remove this test if the examples
    // can't be tested this way, but we recommend including it if possible.

    PluginTest::new("run_internal", RunInternalPlugin.into())?.test_command_examples(&RunInternal)
}

fn main() {
    serve_plugin(&RunInternalPlugin, MsgPackSerializer);
}
