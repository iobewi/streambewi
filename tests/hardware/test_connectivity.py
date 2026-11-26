def test_connectivity(board):
    assert board.generation
    data, cursor, dropped = board.read_uart()
    assert isinstance(data, bytes)
    assert type(cursor) is int and cursor >= len(data)
    assert type(dropped) is int and dropped >= 0
