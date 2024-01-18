//! Contains logic for generating new file names for Instances based on their
//! middleware.

use std::borrow::Cow;

use anyhow::Context;
use rbx_dom_weak::Instance;

use crate::{snapshot::InstanceWithMeta, snapshot_middleware::Middleware};

pub fn name_for_inst<'old>(
    middleware: Middleware,
    new_inst: &Instance,
    old_inst: Option<InstanceWithMeta<'old>>,
) -> anyhow::Result<Cow<'old, str>> {
    if let Some(old_inst) = old_inst {
        if let Some(source) = &old_inst.metadata().instigating_source {
            source
                .path()
                .file_name()
                .and_then(|s| s.to_str())
                .map(Cow::Borrowed)
                .context("sources on the file system should be valid unicode and not be stubs")
        } else {
            anyhow::bail!("members of 'old' trees should have an instigating source!");
        }
    } else {
        Ok(match middleware {
            Middleware::Dir
            | Middleware::CsvDir
            | Middleware::ServerScriptDir
            | Middleware::ClientScriptDir
            | Middleware::ModuleScriptDir => Cow::Owned(new_inst.name.clone()),
            _ => {
                let extension = extension_for_middleware(middleware);
                let name = &new_inst.name;
                if is_valid_file_name(name) {
                    Cow::Owned(format!("{name}.{extension}"))
                } else {
                    anyhow::bail!("name '{name}' is not legal to write to the file system")
                }
            }
        })
    }
}

/// Returns the extension a provided piece of middleware is supposed to use.
fn extension_for_middleware(middleware: Middleware) -> &'static str {
    match middleware {
        Middleware::Csv => "csv",
        Middleware::JsonModel => "model.json",
        Middleware::Json => "json",
        Middleware::ServerScript => "server.luau",
        Middleware::ClientScript => "client.luau",
        Middleware::ModuleScript => "luau",
        Middleware::Project => "project.json",
        Middleware::Rbxm => "rbxm",
        Middleware::Rbxmx => "rbxmx",
        Middleware::Toml => "toml",
        Middleware::Text => "txt",
        // These are manually specified and not `_` to guard against future
        // middleware additions missing this function.
        Middleware::Ignore => unimplemented!("syncback does not work on Ignore middleware"),
        Middleware::Dir
        | Middleware::CsvDir
        | Middleware::ServerScriptDir
        | Middleware::ClientScriptDir
        | Middleware::ModuleScriptDir => {
            unimplemented!("directory middleware requires special treatment")
        }
    }
}

/// A list of file names that are not valid on Windows.
const INVALID_WINDOWS_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// A list of all characters that are outright forbidden to be included
/// in a file's name.
const FORBIDDEN_CHARS: [char; 9] = ['<', '>', ':', '"', '/', '|', '?', '*', '\\'];

/// Returns whether a given name is a valid file name. This takes into account
/// rules for Windows, MacOS, and Linux.
///
/// In practice however, these broadly overlap so the only unexpected behavior
/// is Windows, where there are 22 reserved names.
pub fn is_valid_file_name<S: AsRef<str>>(name: S) -> bool {
    let str = name.as_ref();

    if str.ends_with(' ') || str.ends_with('.') {
        return false;
    }

    for char in str.chars() {
        if char.is_control() || FORBIDDEN_CHARS.contains(&char) {
            return false;
        }
    }

    for forbidden in INVALID_WINDOWS_NAMES {
        if str == forbidden {
            return false;
        }
    }

    true
}
