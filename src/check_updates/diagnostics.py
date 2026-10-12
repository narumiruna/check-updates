"""Scope diagnostic logging to one CLI invocation without changing root logging."""

import logging
from collections.abc import Generator
from contextlib import contextmanager


@contextmanager
def debug_logging(enabled: bool) -> Generator[None]:
    logger = logging.getLogger("check_updates")
    previous_level = logger.level
    previous_propagate = logger.propagate
    handler = logging.StreamHandler()
    handler.setFormatter(logging.Formatter("DEBUG %(name)s: %(message)s"))
    logger.setLevel(logging.DEBUG if enabled else logging.WARNING)
    logger.propagate = False
    if enabled:
        logger.addHandler(handler)
    try:
        yield
    finally:
        if enabled:
            logger.removeHandler(handler)
        handler.close()
        logger.setLevel(previous_level)
        logger.propagate = previous_propagate
