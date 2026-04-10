"""Integration tests for the merge-conflict-assistant LSP server.

These tests launch the real binary over stdio and exercise the LSP protocol
end-to-end, validating that the server follows spec for initialization,
document lifecycle, and diagnostic publishing.
"""

import asyncio
from lsprotocol.types import (
    DidChangeTextDocumentParams,
    DidCloseTextDocumentParams,
    DidOpenTextDocumentParams,
    TextDocumentContentChangeWholeDocument,
    TextDocumentIdentifier,
    TextDocumentItem,
    TextDocumentSyncKind,
    VersionedTextDocumentIdentifier,
)
from pytest_lsp import LanguageClient

from conftest import CONFLICT_DIFF3, CONFLICT_SIMPLE, PLAIN_TEXT

# LSP URIs are just identifiers — the server never reads from the filesystem.
TEST_URI = "file:///fake/test.txt"


async def test_initialize(client: LanguageClient):
    """Server completes the handshake and reports expected capabilities."""
    caps = client.init_result.capabilities

    # text document sync: incremental with open/close
    assert caps.text_document_sync is not None
    assert caps.text_document_sync.open_close is True
    assert caps.text_document_sync.change == TextDocumentSyncKind.Incremental

    # code action provider: quickfix
    assert caps.code_action_provider is not None


async def test_did_open_no_conflicts(client: LanguageClient):
    """Opening a plain file should not produce diagnostics."""
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

    # Give the server a moment to process — if it were going to publish
    # diagnostics it would do so quickly.  We expect nothing.
    await asyncio.sleep(0.5)

    diagnostics = client.diagnostics.get(TEST_URI, [])
    assert len(diagnostics) == 0


async def test_did_open_with_conflicts(client: LanguageClient):
    """Opening a file with conflict markers should publish diagnostics."""
    client.text_document_did_open(
        DidOpenTextDocumentParams(
            text_document=TextDocumentItem(
                uri=TEST_URI,
                language_id="text",
                version=1,
                text=CONFLICT_SIMPLE,
            )
        )
    )

    await client.wait_for_notification("textDocument/publishDiagnostics")

    diagnostics = client.diagnostics.get(TEST_URI, [])
    assert len(diagnostics) > 0, "Expected at least one diagnostic for conflict markers"


async def test_did_open_with_diff3_conflicts(client: LanguageClient):
    """Opening a file with diff3 conflict markers should publish diagnostics."""
    client.text_document_did_open(
        DidOpenTextDocumentParams(
            text_document=TextDocumentItem(
                uri=TEST_URI,
                language_id="text",
                version=1,
                text=CONFLICT_DIFF3,
            )
        )
    )

    await client.wait_for_notification("textDocument/publishDiagnostics")

    diagnostics = client.diagnostics.get(TEST_URI, [])
    assert len(diagnostics) > 0, "Expected at least one diagnostic for diff3 conflict markers"


async def test_did_close(client: LanguageClient):
    """Opening then closing a document should not crash the server."""
    client.text_document_did_open(
        DidOpenTextDocumentParams(
            text_document=TextDocumentItem(
                uri=TEST_URI,
                language_id="text",
                version=1,
                text=CONFLICT_SIMPLE,
            )
        )
    )

    await client.wait_for_notification("textDocument/publishDiagnostics")

    client.text_document_did_close(
        DidCloseTextDocumentParams(
            text_document=TextDocumentIdentifier(uri=TEST_URI),
        )
    )

    # If the server crashes on close, the next operation would fail.
    # Opening a new document exercises that the server is still alive.
    second_uri = "file:///fake/test2.txt"
    client.text_document_did_open(
        DidOpenTextDocumentParams(
            text_document=TextDocumentItem(
                uri=second_uri,
                language_id="text",
                version=1,
                text=CONFLICT_SIMPLE,
            )
        )
    )

    await client.wait_for_notification("textDocument/publishDiagnostics")

    diagnostics = client.diagnostics.get(second_uri, [])
    assert len(diagnostics) > 0


async def test_did_change_introduces_conflicts(client: LanguageClient):
    """A file opened without conflicts that is changed to have them should produce diagnostics."""
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

    # No conflicts yet.
    await asyncio.sleep(0.5)
    assert len(client.diagnostics.get(TEST_URI, [])) == 0

    # Replace the entire content with conflict markers.
    client.text_document_did_change(
        DidChangeTextDocumentParams(
            text_document=VersionedTextDocumentIdentifier(
                uri=TEST_URI,
                version=2,
            ),
            content_changes=[
                TextDocumentContentChangeWholeDocument(text=CONFLICT_SIMPLE),
            ],
        )
    )

    await client.wait_for_notification("textDocument/publishDiagnostics")

    diagnostics = client.diagnostics.get(TEST_URI, [])
    assert len(diagnostics) > 0, "Expected diagnostics after introducing conflict markers"
