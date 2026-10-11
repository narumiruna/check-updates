from functools import partial

import pytest
import tomlkit
from packaging.version import Version
from prompt_toolkit.application import create_app_session
from prompt_toolkit.input import create_pipe_input
from prompt_toolkit.output import DummyOutput

from check_updates import interactive
from check_updates.dependencies import Update
from check_updates.dependencies import dependencies
from check_updates.workspace import read_project


@pytest.fixture
def pending(tmp_path):
    projects = []
    for index, requirements in enumerate((["typer>=1.0", "typer>=1.0"], ["other>=1.0"])):
        path = tmp_path / f"project-{index}.toml"
        document = tomlkit.document()
        document["project"] = {"dependencies": requirements}
        if index == 0:
            document["project"]["optional-dependencies"] = {"cli": ["typer>=1.0"]}
        path.write_text(tomlkit.dumps(document))
        project = read_project(path)
        updates = [
            Update(item, item.requirement.replace("1.0", "2.0"), Version("2.0"))
            for item in dependencies(project.document)
        ]
        projects.append((project, updates))
    return projects


def run_keys(build, keys):
    # Pipe input and dummy output exercise the real event loop without a live TUI.
    with create_pipe_input() as pipe, create_app_session(input=pipe, output=DummyOutput()):
        pipe.send_text(keys)
        application = build()
        application.ttimeoutlen = 0.01
        return application.run()


@pytest.mark.parametrize(
    ("keys", "expected"),
    [
        ("\r", []),
        ("\x1b[B \r", [(0, 0)]),
        ("\x1b[B  \r", []),
        ("\x1b[A \r", [(1, 0)]),
        (" \r", [(0, 0), (0, 1), (0, 2)]),
        ("  \r", []),
        ("\x1b[B \x1b[A \r", [(0, 0), (0, 1), (0, 2)]),
        (" \x1b[B \r", [(0, 1), (0, 2)]),
        ("\x1b[B\x1b[B \r", [(0, 1)]),
        ("\x1b[B\x1b[B\x1b[B \r", [(0, 2)]),
        ("\x1b", None),
    ],
)
def test_selector_keyboard(pending, keys, expected):
    assert run_keys(partial(interactive._selection_application, pending), keys) == expected


def test_selector_ctrl_c(pending):
    with pytest.raises(KeyboardInterrupt):
        run_keys(partial(interactive._selection_application, pending), "\x03")
    # A cancelled application does not prevent a later application from running.
    assert run_keys(partial(interactive._selection_application, pending), "\r") == []


@pytest.mark.parametrize(
    ("keys", "expected"),
    [
        ("\r", False),
        ("n", False),
        ("N", False),
        ("y", True),
        ("Y", True),
        (" \r", True),
        ("\x1b[C\r", True),
        ("\x1b[D\r", True),
        ("\x1b[A\x1b[B\r", False),
        ("\x1b", None),
    ],
)
def test_confirmation_keyboard(keys, expected):
    assert run_keys(interactive._confirmation_application, keys) is expected


def test_confirmation_ctrl_c():
    with pytest.raises(KeyboardInterrupt):
        run_keys(interactive._confirmation_application, "\x03")


def test_selector_labels_and_project_counts(pending):
    selector = interactive._Selector(pending)
    text = "".join(fragment[1] for fragment in selector.text())
    assert "[0/3]" in text
    assert "project.dependencies: typer>=1.0 -> 2.0 [major]" in text
    assert "project.optional-dependencies.cli" in text
    assert str(pending[0][0].path) in text
    selector.toggle()
    assert len(selector.selected) == 3
    assert "[3/3]" in "".join(fragment[1] for fragment in selector.text())


def test_select_updates_preserves_occurrence_identity(pending):
    with create_pipe_input() as pipe, create_app_session(input=pipe, output=DummyOutput()):
        pipe.send_text("\x1b[B\x1b[B \r")
        chosen = interactive.select_updates(pending)
    assert len(chosen) == 1
    assert chosen[0][0] is pending[0][0]
    assert len(chosen[0][1]) == 1
    assert chosen[0][1][0] is pending[0][1][1]


def test_select_updates_escape(pending):
    with create_pipe_input() as pipe, create_app_session(input=pipe, output=DummyOutput()):
        pipe.send_text("\x1b")
        assert interactive.select_updates(pending) == []


@pytest.mark.parametrize(
    ("requirement", "target", "label"),
    [
        ("typer>=1.2.3", "2.0", "major"),
        ("typer>=1.2.3", "1.3.0", "minor"),
        ("typer==1.2.3", "1.2.4", "patch"),
        ("typer~=0.27.1", "0.27.3", "patch"),
        ("typer>=0.27.1", "0.28.0", "minor"),
        ("typer>=1", "1.0.1", "patch"),
        ("typer", "2.0", "unbounded"),
        ("typer>=1!1.0", "2!1.0", "other"),
        ("typer>=1.0rc1", "1.0", "other"),
        ("typer>=1.0.dev1", "1.0", "other"),
        ("typer>=1.0.post1", "1.0.post2", "other"),
        ("typer>=1.0", "1.0+local", "other"),
        ("typer>=1.0.0.1", "1.0.0.2", "other"),
        ("typer>=1.0", "1.0", "other"),
    ],
)
def test_version_change(requirement, target, label):
    document = tomlkit.parse(f"[project]\ndependencies = ['{requirement}']\n")
    dependency = next(dependencies(document))
    assert interactive.version_change(Update(dependency, "unused", Version(target))) == label


@pytest.mark.parametrize("boundary", ["selection", "confirmation"])
def test_closed_pipe_raises_eof(pending, boundary):
    with create_pipe_input() as pipe, create_app_session(input=pipe, output=DummyOutput()):
        app = (
            interactive._selection_application(pending)
            if boundary == "selection"
            else interactive._confirmation_application()
        )
        pipe.close()
        with pytest.raises(EOFError):
            app.run()
