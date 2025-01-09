//! Tie Alumet to a running process.

use std::{
    fs::{self, File, Metadata},
    os::unix::{fs::PermissionsExt, process::ExitStatusExt},
    path::PathBuf,
    process::ExitStatus,
};

use alumet::{
    pipeline::{
        control::ControlMessage,
        elements::source::{self, TriggerMessage},
        matching::TypedElementSelector,
        MeasurementPipeline,
    },
    plugin::event::StartConsumerMeasurement,
    resources::ResourceConsumer,
};
use anyhow::{anyhow, Context, Error};

/// Spawns a child process and waits for it to exit.
pub fn exec_child(external_command: String, args: Vec<String>) -> anyhow::Result<ExitStatus> {
    // Spawn the process.
    let p_result = std::process::Command::new(external_command.clone())
        .args(args.clone())
        .spawn();

    let mut p = match p_result {
        Ok(val) => val,
        Err(e) => match e.kind() {
            std::io::ErrorKind::NotFound => {
                let return_error: String = handle_not_found(external_command, args);
                return Err(anyhow!(return_error));
            }
            std::io::ErrorKind::PermissionDenied => {
                let return_error: String = handle_permission_denied(external_command);
                return Err(anyhow!(return_error));
            }
            _ => {
                return Err(anyhow!("Error in child process"));
            }
        },
    };

    // Notify the plugins that there is a process to observe.
    let pid = p.id();
    log::info!("Child process '{external_command}' spawned with pid {pid}.");
    alumet::plugin::event::start_consumer_measurement()
        .publish(StartConsumerMeasurement(vec![ResourceConsumer::Process { pid }]));

    // Wait for the process to terminate.
    let status = p.wait().context("failed to wait for child process")?;
    Ok(status)
}

fn handle_permission_denied(external_command: String) -> String {
    let file_open_result = File::open(external_command.clone());
    let file_correctly_opened = if let Err(err) = file_open_result {
        // Can't open the file, let's check it's parent
        let current_parent = match find_a_parent_with_perm_issue(external_command.clone()) {
            Ok(parent) => parent,
            Err(err) => return err,
        };
        let metadata: Metadata = current_parent.metadata().expect(&format!("Unable to retrieve metadata of file: {}", current_parent.display()));
        let missing_permissions = check_missing_permissions(metadata.permissions().mode(), 0o555);
        if missing_permissions & 0o500 != 0 || missing_permissions & 0o050 != 0 || missing_permissions & 0o005 != 0 {
            log::warn!(
                "folder '{}' is missing the following permissions:  'rx'",
                current_parent.display()
            );
            log::info!("💡 Hint: try 'chmod +rx {}'", current_parent.display());
        }
        return format!("Error when trying to read the file: {}", external_command.clone());
    } else if let Ok(file) = file_open_result {
        // Can open the file
        file
    } else {
        return "Error when trying to read the file".to_owned();
    };

    // Get file metadata to see missing permissions
    let file_metadata = file_correctly_opened
        .metadata()
        .expect(format!("Unable to retrieve metadata for: {}", external_command).as_str());
    let missing_permissions = check_missing_permissions(file_metadata.permissions().mode(), 0o505);
    let missing_right_str;
    if missing_permissions & 0o500 != 0 || missing_permissions & 0o050 != 0 || missing_permissions & 0o005 != 0 {
        missing_right_str = "rx"
    } else if missing_permissions & 0o400 != 0 || missing_permissions & 0o040 != 0 || missing_permissions & 0o004 != 0 {
        missing_right_str = "r"
    } else if missing_permissions & 0o100 != 0 || missing_permissions & 0o010 != 0 || missing_permissions & 0o001 != 0 {
        missing_right_str = "x"
    } else {
        missing_right_str = "rx"
    }
    log::error!("file '{}' is missing the following permissions:  'x'", external_command);
    log::info!("💡 Hint: try 'chmod +{} {}'",missing_right_str, external_command);
    "Error happened about file's permission".to_string()
}

fn handle_not_found(external_command: String, args: Vec<String>) -> String {
    fn resolve_application_path() -> std::io::Result<PathBuf> {
        std::env::current_exe()?.canonicalize()
    }
    log::error!("Command '{}' not found", external_command);
    let directory_entries_iter = match fs::read_dir(".") {
        Ok(directory) => directory,
        Err(err) => {
            panic!("Error when trying to read current directory: {}", err);
        }
    };
    let app_path = resolve_application_path()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_owned()))
        .unwrap_or(String::from("path/to/agent"));

    let mut lowest_distance = usize::MAX;
    let mut best_element = None;

    for entry_result in directory_entries_iter {
        let entry = entry_result.unwrap();
        let entry_type = entry.file_type().unwrap();
        if entry_type.is_file() {
            let entry_string = entry.file_name().into_string().unwrap();
            let distance = super::word_distance::distance_with_adjacent_transposition(
                external_command
                .strip_prefix("./")
                .unwrap_or(&external_command)
                .to_string(),
                entry_string.clone(),
            );
            if distance < 3 && distance < lowest_distance {
                lowest_distance = distance;
                best_element = Some((entry_string, distance));
            }
        }
    }
    match best_element {
        Some((element, distance)) => {
            let argument_list = args.iter().map(|arg| {
                if arg.contains(' ') {
                   format!("\"{}\"", arg)
               } else {
                   arg.to_string()
               }
           }).collect::<Vec<_>>().join(" ");
            if distance == 0 {
                log::info!(
                    "💡 Hint: A file named '{}' exists in the current directory. Prepend ./ to execute it.",
                    element
                );
                log::info!("Example: {} exec ./{} {}", app_path, element, argument_list);
            } else {
                log::info!("💡 Hint: Did you mean ./{} {}", element, argument_list);
            }
        }
        None => {
            log::warn!("💡 Hint: No matching file exists in the current directory. Please try again we a different one.");
        }
    }
    "Sorry but the file was not found".to_string()
}

fn check_missing_permissions(current_permissions: u32, required_permissions: u32) -> u32 {
    let mut missing = 0;

    // Check read, write and execute permissions for the user
    if (required_permissions & 0o400) != 0 && (current_permissions & 0o400) == 0 {
        missing |= 0o400;
    }
    if (required_permissions & 0o200) != 0 && (current_permissions & 0o200) == 0 {
        missing |= 0o200;
    }
    if (required_permissions & 0o100) != 0 && (current_permissions & 0o100) == 0 {
        missing |= 0o100;
    }

    // Check read, write and execute permissions for the group
    if (required_permissions & 0o040) != 0 && (current_permissions & 0o040) == 0 {
        missing |= 0o040;
    }
    if (required_permissions & 0o020) != 0 && (current_permissions & 0o020) == 0 {
        missing |= 0o020;
    }
    if (required_permissions & 0o010) != 0 && (current_permissions & 0o010) == 0 {
        missing |= 0o010;
    }

    // Check read, write and execute permissions for others
    if (required_permissions & 0o004) != 0 && (current_permissions & 0o004) == 0 {
        missing |= 0o004;
    }
    if (required_permissions & 0o002) != 0 && (current_permissions & 0o002) == 0 {
        missing |= 0o002;
    }
    if (required_permissions & 0o001) != 0 && (current_permissions & 0o001) == 0 {
        missing |= 0o001;
    }

    missing
}

fn find_a_parent_with_perm_issue(path: String) -> anyhow::Result<std::path::PathBuf, String> {
    // Current parent can change if a parent of the parent don't have the correct rights
    let mut current_parent = match std::path::Path::new(&path).parent() {
        Some(parent) => parent,
        None => return Err("".to_string()),
    };
    // Through this loop I will iterate over parent of parent until I can retrieve metadata, it will show the first folder
    // that I can't execute and suggest to the user to grant execution rights.
    loop {
        match current_parent.metadata() {
            Ok(_) => return Ok(current_parent.to_path_buf()),
            Err(_) => {
                current_parent = match current_parent.parent() {
                    Some(parent) => parent,
                    None => return Err("Unable to retrieve a parent for your file".to_string()),
                }
            }
        }
    }
}

// Triggers one measurement (on all sources that support manual trigger).
pub fn trigger_measurement_now(pipeline: &MeasurementPipeline) -> anyhow::Result<()> {
    let control_handle = pipeline.control_handle();
    let send_task = control_handle.send(ControlMessage::Source(source::ControlMessage::TriggerManually(
        TriggerMessage {
            selector: TypedElementSelector::all(),
        },
    )));
    pipeline
        .async_runtime()
        .block_on(send_task)
        .context("failed to send TriggerMessage")
}
