use std::io::IsTerminal;

use comfy_table::{Cell, Color, ContentArrangement, Table, presets::UTF8_FULL};
use serde_json::{Value, json};

use crate::changes::safe_text;
use crate::dependencies::{Update, version_change};
use crate::workspace::Project;

pub type Pending = Vec<(Project, Vec<Update>)>;

pub fn json_updates(pending: &Pending) -> Value {
    let updates: Vec<Value> = pending
        .iter()
        .flat_map(|(project, updates)| {
            updates.iter().map(move |update| {
                json!({
                    "file": project.path.to_string_lossy(),
                    "requirement": update.dependency.requirement,
                    "replacement": update.replacement,
                    "version": update.version.to_string(),
                    "section": update.dependency.section,
                    "index": update.dependency.index,
                    "change": version_change(update),
                    "release_url": update.release_link,
                    "project_release_url": update.project_link,
                })
            })
        })
        .collect();
    json!({"schema_version": 1, "updates": updates})
}

pub fn show_updates(pending: &Pending, upgrade: bool, workspace: bool) {
    println!("Dependency updates\n");
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    for (project, updates) in pending {
        println!("{}:", safe_text(&project.path.to_string_lossy(), false));
        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .set_content_arrangement(ContentArrangement::Dynamic);
        if let Ok((width, _)) = crossterm::terminal::size() {
            table.set_width(width.max(40));
        }
        table.set_header(["Current requirement", "Target", "Change", "Section"]);
        for update in updates {
            let label = version_change(update);
            let mut change = Cell::new(label);
            if color {
                change = change.fg(match label {
                    "major" => Color::Red,
                    "minor" => Color::Yellow,
                    "patch" => Color::Green,
                    "unbounded" => Color::Cyan,
                    _ => Color::Magenta,
                });
            }
            table.add_row(vec![
                Cell::new(safe_text(&update.dependency.requirement, false)),
                Cell::new(&update.version),
                change,
                Cell::new(safe_text(&update.dependency.section.join("."), false)),
            ]);
        }
        println!("{table}\n");
    }
    let count: usize = pending.iter().map(|(_, updates)| updates.len()).sum();
    println!(
        "{count} {} with updates in {} {}.",
        if count == 1 {
            "dependency"
        } else {
            "dependencies"
        },
        pending.len(),
        if pending.len() == 1 { "file" } else { "files" }
    );
    println!(
        "Change labels compare requirement versions, not installed versions or compatibility."
    );
    if !upgrade {
        let suffix = if workspace { " -w" } else { "" };
        println!(
            "No files changed. Use -u{suffix} to apply all updates or -i{suffix} to select updates."
        );
    }
}
