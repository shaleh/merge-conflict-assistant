"""Test that code action requests always get a response.

The LSP spec requires every request to receive a response.  When no
conflicts exist the server must reply with an empty list — not silence.
A missing response causes editors like Helix to block until timeout
before showing code actions from other LSP servers.
"""

import asyncio

from lsprotocol.types import (
    CodeActionContext,
    CodeActionParams,
    DidOpenTextDocumentParams,
    Position,
    Range,
    TextDocumentIdentifier,
    TextDocumentItem,
)
from pytest_lsp import LanguageClient

from conftest import CONFLICT_SIMPLE, CONFLICT_JJ_SNAPSHOT, CONFLICT_MIXED_FORMAT, PLAIN_TEXT

TEST_URI = "file:///fake/code_action_test.txt"


async def test_code_action_on_file_without_conflicts_returns_empty_list(client: LanguageClient):
    """A code action request on a conflict-free file must return an empty list, not hang."""
    client.text_document_did_open(
        DidOpenTextDocumentParams(
            text_document=TextDocumentItem(
                uri=TEST_URI,
                language_id="text",
                version=1,
                text=PLAIN_TEXT,
            )
        )
    )

    # Give the server a moment to process the didOpen.
    await asyncio.sleep(0.5)

    actions = await asyncio.wait_for(
        asyncio.wrap_future(client.text_document_code_action(
            CodeActionParams(
                text_document=TextDocumentIdentifier(uri=TEST_URI),
                range=Range(
                    start=Position(line=0, character=0),
                    end=Position(line=0, character=1),
                ),
                context=CodeActionContext(diagnostics=[]),
            )
        )),
        timeout=3,
    )

    assert actions is not None
    assert len(actions) == 0


async def test_code_action_outside_conflict_range_returns_empty_list(client: LanguageClient):
    """A code action request outside any conflict region must return an empty list."""
    # Use a file with conflicts but request actions on a line outside them.
    text_with_surrounding = "line before\n" + CONFLICT_SIMPLE + "line after\n"
    client.text_document_did_open(
        DidOpenTextDocumentParams(
            text_document=TextDocumentItem(
                uri=TEST_URI,
                language_id="text",
                version=1,
                text=text_with_surrounding,
            )
        )
    )

    await client.wait_for_notification("textDocument/publishDiagnostics")

    # Line 0 is "line before" — outside the conflict.
    actions = await asyncio.wait_for(
        asyncio.wrap_future(client.text_document_code_action(
            CodeActionParams(
                text_document=TextDocumentIdentifier(uri=TEST_URI),
                range=Range(
                    start=Position(line=0, character=0),
                    end=Position(line=0, character=1),
                ),
                context=CodeActionContext(diagnostics=[]),
            )
        )),
        timeout=3,
    )

    assert actions is not None
    assert len(actions) == 0


async def test_code_action_outside_snapshot_conflict_returns_empty(client: LanguageClient):
    """A code action request at a position outside the snapshot conflict region must
    return an empty list — the server must not offer actions for unrelated document positions."""
    text_with_surrounding = "line before conflict\n" + CONFLICT_JJ_SNAPSHOT + "line after conflict\n"
    client.text_document_did_open(
        DidOpenTextDocumentParams(
            text_document=TextDocumentItem(
                uri=TEST_URI,
                language_id="text",
                version=1,
                text=text_with_surrounding,
            )
        )
    )

    await client.wait_for_notification("textDocument/publishDiagnostics")

    actions = await asyncio.wait_for(
        asyncio.wrap_future(client.text_document_code_action(
            CodeActionParams(
                text_document=TextDocumentIdentifier(uri=TEST_URI),
                range=Range(
                    start=Position(line=0, character=0),
                    end=Position(line=0, character=1),
                ),
                context=CodeActionContext(diagnostics=[]),
            )
        )),
        timeout=3,
    )

    assert actions is not None
    assert len(actions) == 0, (
        f"Expected empty list for position outside snapshot conflict, got {actions}"
    )


async def test_code_action_in_mixed_format_file_returns_empty(client: LanguageClient):
    """A code action request at any range in a mixed-format file must return an
    empty list — no resolution actions are offered when the file mixes
    diff3-style and jj snapshot conflict markers."""
    client.text_document_did_open(
        DidOpenTextDocumentParams(
            text_document=TextDocumentItem(
                uri=TEST_URI,
                language_id="text",
                version=1,
                text=CONFLICT_MIXED_FORMAT,
            )
        )
    )

    await client.wait_for_notification("textDocument/publishDiagnostics")

    actions = await asyncio.wait_for(
        asyncio.wrap_future(client.text_document_code_action(
            CodeActionParams(
                text_document=TextDocumentIdentifier(uri=TEST_URI),
                range=Range(
                    start=Position(line=2, character=0),
                    end=Position(line=2, character=1),
                ),
                context=CodeActionContext(diagnostics=[]),
            )
        )),
        timeout=3,
    )

    assert actions is not None
    assert len(actions) == 0, (
        f"Expected empty action list for mixed-format file, got {actions}"
    )
