import os
from collections.abc import Iterator

import pytest

from idlab_client import Board, IDLabClient


@pytest.fixture
def board() -> Iterator[Board]:
    name = os.environ.get("IDLAB_BOARD")
    token = os.environ.get("IDLAB_BOARD_TOKEN")
    if not name or not token:
        pytest.fail("Set IDLAB_BOARD and IDLAB_BOARD_TOKEN before running hardware tests.", pytrace=False)
    with IDLabClient(token="", timeout=10).control(name, token) as connected:
        yield connected
