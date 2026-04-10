"""Test the full resolve-conflict round trip.

Opens a file with conflict markers, requests code actions, applies one of
the edits back to the server, and verifies the conflict diagnostics clear.
"""

import asyncio

from lsprotocol.types import (
    CodeActionContext,
    CodeActionParams,
    DidOpenTextDocumentParams,
    DidChangeTextDocumentParams,
    Range,
    Position,
    TextDocumentContentChangePartial,
    TextDocumentIdentifier,
    TextDocumentItem,
    VersionedTextDocumentIdentifier,
)
from pytest_lsp import LanguageClient

from conftest import CONFLICT_SIMPLE

TEST_URI = "file:///fake/resolve_test.txt"


async def test_resolve_conflict_choosing_head_clears_diagnostics(client: LanguageClient):
    """Applying a code action edit should clear the conflict diagnostics."""
    # 1. Open a file with a single conflict.
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
    assert len(diagnostics) == 1, f"Expected 1 diagnostic, got {len(diagnostics)}"

    # 2. Request code actions at a position inside the conflict.
    actions = await asyncio.wrap_future(client.text_document_code_action(
        CodeActionParams(
            text_document=TextDocumentIdentifier(uri=TEST_URI),
            range=Range(
                start=Position(line=1, character=0),
                end=Position(line=1, character=1),
            ),
            context=CodeActionContext(diagnostics=diagnostics),
        )
    ))

    assert actions is not None and len(actions) > 0, "Expected at least one code action"

    # 3. Pick the first action ("Keep HEAD") and extract its edit.
    action = actions[0]
    assert action.title == "Keep HEAD"
    assert action.edit is not None
    changes = action.edit.changes
    assert changes is not None
    edits = changes[TEST_URI]
    assert len(edits) == 1

    edit = edits[0]

    # 4. Apply the edit back to the server as an incremental change.
    client.text_document_did_change(
        DidChangeTextDocumentParams(
            text_document=VersionedTextDocumentIdentifier(
                uri=TEST_URI,
                version=2,
            ),
            content_changes=[
                TextDocumentContentChangePartial(
                    range=edit.range,
                    text=edit.new_text,
                ),
            ],
        )
    )

    await client.wait_for_notification("textDocument/publishDiagnostics")

    # 5. Diagnostics should now be empty — the conflict is resolved.
    diagnostics = client.diagnostics.get(TEST_URI, [])
    assert len(diagnostics) == 0, f"Expected 0 diagnostics after resolve, got {diagnostics}"


async def test_resolve_conflict_choosing_drop_all_clears_diagnostics(client: LanguageClient):
    """Applying drop all should clear the conflict diagnostics."""
    # 1. Open a file with a single conflict.
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
    assert len(diagnostics) == 1, f"Expected 1 diagnostic, got {len(diagnostics)}"

    # 2. Request code actions at a position inside the conflict.
    actions = await asyncio.wrap_future(client.text_document_code_action(
        CodeActionParams(
            text_document=TextDocumentIdentifier(uri=TEST_URI),
            range=Range(
                start=Position(line=1, character=0),
                end=Position(line=1, character=1),
            ),
            context=CodeActionContext(diagnostics=diagnostics),
        )
    ))

    assert actions is not None and len(actions) > 0, "Expected at least one code action"

    # 3. Pick the last action ("Drop all") and extract its edit.
    action = actions[-1]
    assert action.title == "Drop all"
    assert action.edit is not None
    changes = action.edit.changes
    assert changes is not None
    edits = changes[TEST_URI]
    assert len(edits) == 1

    edit = edits[0]

    # 4. Apply the edit back to the server as an incremental change.
    client.text_document_did_change(
        DidChangeTextDocumentParams(
            text_document=VersionedTextDocumentIdentifier(
                uri=TEST_URI,
                version=2,
            ),
            content_changes=[
                TextDocumentContentChangePartial(
                    range=edit.range,
                    text=edit.new_text,
                ),
            ],
        )
    )

    await client.wait_for_notification("textDocument/publishDiagnostics")

    # 5. Diagnostics should now be empty — the conflict is resolved.
    diagnostics = client.diagnostics.get(TEST_URI, [])
    assert len(diagnostics) == 0, f"Expected 0 diagnostics after resolve, got {diagnostics}"
