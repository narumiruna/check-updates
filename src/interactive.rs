use std::collections::HashSet;
use std::io::{Write, stdout};
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    cursor::{Hide, MoveTo, MoveToColumn, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    style::{Attribute, Print, SetAttribute},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};

use crate::changes::safe_text;
use crate::dependencies::version_change;
use crate::output::Pending;

struct Screen;

impl Screen {
    fn open() -> Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self;
        execute!(stdout(), EnterAlternateScreen, Hide)?;
        Ok(guard)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(
            stdout(),
            SetAttribute(Attribute::Reset),
            Show,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

fn read_key() -> Result<KeyEvent> {
    loop {
        crate::check_interrupt()?;
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
        {
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return Err(crate::Interrupted.into());
            }
            return Ok(key);
        }
    }
}

#[derive(Default)]
struct Selector {
    rows: Vec<(usize, Option<usize>)>,
    selected: HashSet<(usize, usize)>,
    cursor: usize,
}

impl Selector {
    fn new(pending: &Pending) -> Self {
        let mut selector = Self::default();
        for (project, (_, updates)) in pending.iter().enumerate() {
            selector.rows.push((project, None));
            selector
                .rows
                .extend((0..updates.len()).map(|index| (project, Some(index))));
        }
        selector
    }

    fn toggle(&mut self, pending: &Pending) {
        let (project, index) = self.rows[self.cursor];
        if let Some(index) = index {
            if !self.selected.remove(&(project, index)) {
                self.selected.insert((project, index));
            }
        } else {
            let children: Vec<_> = (0..pending[project].1.len())
                .map(|index| (project, index))
                .collect();
            let clear = children.iter().all(|child| self.selected.contains(child));
            for child in children {
                if clear {
                    self.selected.remove(&child);
                } else {
                    self.selected.insert(child);
                }
            }
        }
    }

    fn render(&self, pending: &Pending) -> Result<()> {
        let (width, height) = terminal::size()?;
        let visible = usize::from(height.saturating_sub(3).max(1));
        let start = self.cursor.saturating_sub(visible - 1);
        let mut output = stdout();
        execute!(
            output,
            MoveTo(0, 0),
            Clear(ClearType::All),
            Print(format!(
                "Select dependency updates ({} selected). Labels do not prove compatibility.\r\nArrows: move | Space: toggle entry/project | Enter: review | Esc: cancel | Ctrl+C: interrupt\r\n",
                self.selected.len()
            ))
        )?;
        for (row, &(project_index, index)) in self.rows.iter().enumerate().skip(start).take(visible)
        {
            let (project, updates) = &pending[project_index];
            let label = if let Some(index) = index {
                let update = &updates[index];
                format!(
                    "  [{}] {}: {} -> {} [{}]",
                    if self.selected.contains(&(project_index, index)) {
                        "x"
                    } else {
                        " "
                    },
                    update.dependency.section.join("."),
                    update.dependency.requirement,
                    update.version,
                    version_change(update)
                )
            } else {
                let selected = (0..updates.len())
                    .filter(|index| self.selected.contains(&(project_index, *index)))
                    .count();
                format!(
                    "[{selected}/{}] {} (select/clear project)",
                    updates.len(),
                    project.path.display()
                )
            };
            // A single screen row per occurrence keeps scrolling and selection aligned.
            let label: String = safe_text(&label, false)
                .chars()
                .take(usize::from(width.saturating_sub(1)))
                .collect();
            if row == self.cursor {
                execute!(output, SetAttribute(Attribute::Reverse))?;
            }
            execute!(
                output,
                Print(label),
                SetAttribute(Attribute::Reset),
                Print("\r\n")
            )?;
        }
        output.flush()?;
        Ok(())
    }
}

pub fn select_updates(pending: &Pending) -> Result<Pending> {
    let _screen = Screen::open()?;
    let mut selector = Selector::new(pending);
    if selector.rows.is_empty() {
        return Ok(Vec::new());
    }
    loop {
        selector.render(pending)?;
        match read_key()?.code {
            KeyCode::Up => {
                selector.cursor = (selector.cursor + selector.rows.len() - 1) % selector.rows.len()
            }
            KeyCode::Down => selector.cursor = (selector.cursor + 1) % selector.rows.len(),
            KeyCode::Char(' ') => selector.toggle(pending),
            KeyCode::Esc => return Ok(Vec::new()),
            KeyCode::Enter => break,
            _ => (),
        }
    }
    Ok(pending
        .iter()
        .enumerate()
        .filter_map(|(project_index, (project, updates))| {
            let chosen: Vec<_> = updates
                .iter()
                .enumerate()
                .filter(|(index, _)| selector.selected.contains(&(project_index, *index)))
                .map(|(_, update)| update.clone())
                .collect();
            (!chosen.is_empty()).then(|| (project.clone(), chosen))
        })
        .collect())
}

struct Confirmation;

impl Drop for Confirmation {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            stdout(),
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            Show
        );
    }
}

pub fn confirm_changes() -> Result<bool> {
    // Keep the diff visible above the confirmation instead of opening another screen.
    stdout().flush()?;
    terminal::enable_raw_mode()?;
    let _guard = Confirmation;
    let mut accepted = false;
    loop {
        execute!(
            stdout(),
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            Print(format!(
                "Apply these changes? {} (Arrows/Space: choose | Enter: confirm | y/n | Esc: cancel)",
                if accepted { "No / [Yes]" } else { "[No] / Yes" }
            ))
        )?;
        stdout().flush()?;
        match read_key()?.code {
            KeyCode::Char('y' | 'Y') => return Ok(true),
            KeyCode::Esc | KeyCode::Char('n' | 'N') => return Ok(false),
            KeyCode::Enter => return Ok(accepted),
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down | KeyCode::Char(' ') => {
                accepted = !accepted
            }
            _ => (),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dependencies::{Dependency, Update};
    use crate::workspace::Project;

    #[test]
    fn project_toggle_and_occurrence_toggle_are_independent() {
        let update = Update {
            dependency: Dependency {
                requirement: "demo>=1".into(),
                section: vec!["project".into(), "dependencies".into()],
                index: 0,
            },
            replacement: "demo>=2".into(),
            version: "2".parse().unwrap(),
            release_link: None,
            project_link: None,
        };
        let pending = vec![(
            Project {
                path: "pyproject.toml".into(),
                document: Default::default(),
                original: Vec::new(),
            },
            vec![update.clone(), update],
        )];
        let mut selector = Selector::new(&pending);
        assert!(selector.selected.is_empty());
        selector.toggle(&pending);
        assert_eq!(selector.selected.len(), 2);
        selector.cursor = 1;
        selector.toggle(&pending);
        assert_eq!(selector.selected.len(), 1);
        assert!(selector.selected.contains(&(0, 1)));
        selector.cursor = 0;
        selector.toggle(&pending);
        assert_eq!(selector.selected.len(), 2);
        selector.toggle(&pending);
        assert!(selector.selected.is_empty());
    }
}
