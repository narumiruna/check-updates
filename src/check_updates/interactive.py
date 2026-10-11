"""Terminal selection and confirmation using prompt-toolkit's event loop."""

from dataclasses import dataclass
from dataclasses import field

from prompt_toolkit.application import Application
from prompt_toolkit.formatted_text import StyleAndTextTuples
from prompt_toolkit.key_binding import KeyBindings
from prompt_toolkit.key_binding import KeyPressEvent
from prompt_toolkit.layout import HSplit
from prompt_toolkit.layout import Layout
from prompt_toolkit.layout import Window
from prompt_toolkit.layout.controls import FormattedTextControl

from check_updates.dependencies import Update
from check_updates.dependencies import version_change as version_change
from check_updates.workspace import Project


def _cancellation_bindings() -> KeyBindings:
    bindings = KeyBindings()

    @bindings.add("escape", eager=True)
    def cancel(event: KeyPressEvent) -> None:
        event.app.exit(result=None)

    @bindings.add("c-c")
    @bindings.add("<sigint>")
    def interrupt(event: KeyPressEvent) -> None:
        event.app.exit(exception=KeyboardInterrupt())

    return bindings


@dataclass
class _Selector:
    pending: list[tuple[Project, list[Update]]]
    cursor: int = 0
    selected: set[tuple[int, int]] = field(default_factory=set)
    rows: list[tuple[int, int | None]] = field(init=False)

    def __post_init__(self) -> None:
        self.rows = []
        for project_index, (_project, updates) in enumerate(self.pending):
            self.rows.append((project_index, None))
            self.rows.extend((project_index, index) for index in range(len(updates)))

    def toggle(self) -> None:
        project_index, index = self.rows[self.cursor]
        if index is None:
            children = {(project_index, child) for child in range(len(self.pending[project_index][1]))}
            if children <= self.selected:
                self.selected.difference_update(children)
            else:
                self.selected.update(children)
        else:
            item = (project_index, index)
            self.selected.symmetric_difference_update({item})

    def text(self) -> StyleAndTextTuples:
        tokens: StyleAndTextTuples = []
        for row_index, (project_index, index) in enumerate(self.rows):
            project, updates = self.pending[project_index]
            if index is None:
                count = sum((project_index, child) in self.selected for child in range(len(updates)))
                label = f"[{count}/{len(updates)}] {project.path} (select/clear project)"
            else:
                update = updates[index]
                checked = "x" if (project_index, index) in self.selected else " "
                section = ".".join(update.dependency.section)
                label = (
                    f"  [{checked}] {section}: {update.dependency.requirement} "
                    f"-> {update.version} [{version_change(update)}]"
                )
            if row_index == self.cursor:
                tokens.append(("[SetCursorPosition]", ""))
            tokens.append(("reverse" if row_index == self.cursor else "", label + "\n"))
        return tokens


def _selection_application(pending: list[tuple[Project, list[Update]]]) -> Application[list[tuple[int, int]] | None]:
    selector = _Selector(pending)
    bindings = _cancellation_bindings()

    @bindings.add("up")
    def move_up(_event: KeyPressEvent) -> None:
        selector.cursor = (selector.cursor - 1) % len(selector.rows)

    @bindings.add("down")
    def move_down(_event: KeyPressEvent) -> None:
        selector.cursor = (selector.cursor + 1) % len(selector.rows)

    @bindings.add(" ")
    def toggle(_event: KeyPressEvent) -> None:
        selector.toggle()

    @bindings.add("enter")
    def accept(event: KeyPressEvent) -> None:
        event.app.exit(result=sorted(selector.selected))

    header = FormattedTextControl(
        lambda: (
            f"Select dependency updates ({len(selector.selected)} selected). Labels do not prove compatibility.\n"
            "Arrows: move | Space: toggle entry/project | Enter: review | Escape: cancel | Ctrl+C: interrupt"
        )
    )
    choices = FormattedTextControl(selector.text, focusable=True)
    layout = Layout(
        HSplit([Window(header, dont_extend_height=True, wrap_lines=True), Window(choices, wrap_lines=True)]), choices
    )
    return Application(layout=layout, key_bindings=bindings, erase_when_done=True)


def select_updates(pending: list[tuple[Project, list[Update]]]) -> list[tuple[Project, list[Update]]]:
    selected = set(_selection_application(pending).run() or [])
    result = []
    for project_index, (project, updates) in enumerate(pending):
        chosen = [update for index, update in enumerate(updates) if (project_index, index) in selected]
        if chosen:
            result.append((project, chosen))
    return result


def _confirmation_application() -> Application[bool | None]:
    bindings = _cancellation_bindings()
    accepted = False

    @bindings.add("left")
    @bindings.add("right")
    @bindings.add("up")
    @bindings.add("down")
    @bindings.add(" ")
    def toggle(_event: KeyPressEvent) -> None:
        nonlocal accepted
        accepted = not accepted

    @bindings.add("y")
    @bindings.add("Y")
    def yes(event: KeyPressEvent) -> None:
        event.app.exit(result=True)

    @bindings.add("n")
    @bindings.add("N")
    def no(event: KeyPressEvent) -> None:
        event.app.exit(result=False)

    @bindings.add("enter")
    def accept(event: KeyPressEvent) -> None:
        event.app.exit(result=accepted)

    control = FormattedTextControl(
        lambda: (
            f"Apply these changes? {'No / [Yes]' if accepted else '[No] / Yes'} "
            "(Arrows/Space: choose | Enter: confirm | y/n | Escape: cancel)"
        ),
        focusable=True,
    )
    return Application(layout=Layout(Window(control, wrap_lines=True)), key_bindings=bindings, erase_when_done=True)


def confirm_changes() -> bool:
    return bool(_confirmation_application().run())
