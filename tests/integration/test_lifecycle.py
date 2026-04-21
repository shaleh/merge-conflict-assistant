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

from conftest import (
    CONFLICT_DIFF3,
    CONFLICT_JJ_SNAPSHOT,
    CONFLICT_JJ_SNAPSHOT_3WAY,
    CONFLICT_SIMPLE,
    PLAIN_TEXT,
)

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


async def test_diagnostic_published_on_jj_snapshot_open(client: LanguageClient):
    """Opening a jj snapshot conflict file publishes exactly one ERROR diagnostic
    with source 'merge' and message 'jj snapshot conflict'."""
    client.text_document_did_open(
        DidOpenTextDocumentParams(
            text_document=TextDocumentItem(
                uri=TEST_URI,
                language_id="text",
                version=1,
                text=CONFLICT_JJ_SNAPSHOT,
            )
        )
    )

    await client.wait_for_notification("textDocument/publishDiagnostics")

    diagnostics = client.diagnostics.get(TEST_URI, [])
    assert len(diagnostics) == 1, f"Expected exactly 1 diagnostic, got {len(diagnostics)}"
    d = diagnostics[0]
    assert d.source == "merge", f"Expected source='merge', got {d.source!r}"
    assert d.message == "jj snapshot conflict", f"Expected message='jj snapshot conflict', got {d.message!r}"
    from lsprotocol.types import DiagnosticSeverity
    assert d.severity == DiagnosticSeverity.Error, f"Expected ERROR severity, got {d.severity}"


async def test_incremental_edit_clears_jj_snapshot_diagnostic(client: LanguageClient):
    """Replacing a jj snapshot conflict with plain text clears the diagnostic."""
    client.text_document_did_open(
        DidOpenTextDocumentParams(
            text_document=TextDocumentItem(
                uri=TEST_URI,
                language_id="text",
                version=1,
                text=CONFLICT_JJ_SNAPSHOT,
            )
        )
    )

    await client.wait_for_notification("textDocument/publishDiagnostics")
    diagnostics = client.diagnostics.get(TEST_URI, [])
    assert len(diagnostics) == 1, f"Expected 1 diagnostic before resolution, got {len(diagnostics)}"

    client.text_document_did_change(
        DidChangeTextDocumentParams(
            text_document=VersionedTextDocumentIdentifier(
                uri=TEST_URI,
                version=2,
            ),
            content_changes=[
                TextDocumentContentChangeWholeDocument(text=PLAIN_TEXT),
            ],
        )
    )

    await client.wait_for_notification("textDocument/publishDiagnostics")

    diagnostics = client.diagnostics.get(TEST_URI, [])
    assert len(diagnostics) == 0, f"Expected 0 diagnostics after clearing conflict, got {diagnostics}"


async def test_diagnostic_published_on_3way_jj_snapshot_open(client: LanguageClient):
    """An octopus (3-parent) jj snapshot conflict — 3 sides with 2 interleaved
    bases — publishes exactly one ERROR diagnostic, same shape as the 2-way case."""
    client.text_document_did_open(
        DidOpenTextDocumentParams(
            text_document=TextDocumentItem(
                uri=TEST_URI,
                language_id="text",
                version=1,
                text=CONFLICT_JJ_SNAPSHOT_3WAY,
            )
        )
    )

    await client.wait_for_notification("textDocument/publishDiagnostics")

    diagnostics = client.diagnostics.get(TEST_URI, [])
    assert len(diagnostics) == 1, f"Expected exactly 1 diagnostic, got {len(diagnostics)}"
    d = diagnostics[0]
    assert d.source == "merge", f"Expected source='merge', got {d.source!r}"
    assert d.message == "jj snapshot conflict", (
        f"Expected message='jj snapshot conflict', got {d.message!r}"
    )
    from lsprotocol.types import DiagnosticSeverity
    assert d.severity == DiagnosticSeverity.Error, f"Expected ERROR severity, got {d.severity}"
