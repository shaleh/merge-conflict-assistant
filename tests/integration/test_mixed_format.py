"""Integration tests for mixed-format file handling.

A file containing both a diff3-style conflict block and a jj snapshot conflict
block must produce exactly one ERROR diagnostic (at the second block's opening
marker line) and must not offer any code actions.
"""

import asyncio

from lsprotocol.types import (
    CodeActionContext,
    CodeActionParams,
    DiagnosticSeverity,
    DidOpenTextDocumentParams,
    Position,
    Range,
    TextDocumentIdentifier,
    TextDocumentItem,
)
from pytest_lsp import LanguageClient

from conftest import CONFLICT_MIXED_FORMAT

TEST_URI = "file:///fake/mixed_format_test.txt"


async def test_mixed_format_file_publishes_single_error_diagnostic(client: LanguageClient):
    """Opening a file with both diff3-style and jj snapshot conflict blocks must
    publish exactly one ERROR diagnostic with source 'merge', the mixed-format
    message, and a range starting at the second block's opening marker line
    (line 7, character 0) ending at (line 8, character 0)."""
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

    diagnostics = client.diagnostics.get(TEST_URI, [])
    assert len(diagnostics) == 1, (
        f"Expected exactly 1 diagnostic for a mixed-format file, got {len(diagnostics)}: "
        f"{diagnostics}"
    )

    d = diagnostics[0]

    assert d.severity == DiagnosticSeverity.Error, (
        f"Expected ERROR severity, got {d.severity}"
    )
    assert d.source == "merge", (
        f"Expected source='merge', got {d.source!r}"
    )
    assert d.message == "file contains mixed diff3-style and jj snapshot conflict markers", (
        f"Unexpected diagnostic message: {d.message!r}"
    )
    assert d.range.start.line == 7, (
        f"Expected diagnostic to start at line 7 (the second <<<<<<< line), got {d.range.start.line}"
    )
    assert d.range.start.character == 0, (
        f"Expected diagnostic to start at character 0, got {d.range.start.character}"
    )
    assert d.range.end.line == 8, (
        f"Expected diagnostic end line to be 8 (second_line + 1), got {d.range.end.line}"
    )
    assert d.range.end.character == 0, (
        f"Expected diagnostic end character to be 0, got {d.range.end.character}"
    )


async def test_mixed_format_file_offers_no_code_actions(client: LanguageClient):
    """No resolution code actions must be offered for any range in a
    mixed-format file, including ranges that fall inside one of the individual
    conflict blocks."""
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

    range_inside_diff3_block = Range(
        start=Position(line=2, character=0),
        end=Position(line=2, character=1),
    )

    actions = await asyncio.wait_for(
        asyncio.wrap_future(client.text_document_code_action(
            CodeActionParams(
                text_document=TextDocumentIdentifier(uri=TEST_URI),
                range=range_inside_diff3_block,
                context=CodeActionContext(diagnostics=[]),
            )
        )),
        timeout=3,
    )

    assert actions is not None
    assert len(actions) == 0, (
        f"Expected empty action list for mixed-format file (range inside diff3 block), "
        f"got {actions}"
    )

    range_inside_snapshot_block = Range(
        start=Position(line=9, character=0),
        end=Position(line=9, character=1),
    )

    actions2 = await asyncio.wait_for(
        asyncio.wrap_future(client.text_document_code_action(
            CodeActionParams(
                text_document=TextDocumentIdentifier(uri=TEST_URI),
                range=range_inside_snapshot_block,
                context=CodeActionContext(diagnostics=[]),
            )
        )),
        timeout=3,
    )

    assert actions2 is not None
    assert len(actions2) == 0, (
        f"Expected empty action list for mixed-format file (range inside snapshot block), "
        f"got {actions2}"
    )
