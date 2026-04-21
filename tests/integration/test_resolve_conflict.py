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

from conftest import CONFLICT_SIMPLE, CONFLICT_JJ_SNAPSHOT, CONFLICT_JJ_SNAPSHOT_3WAY

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
    assert len(diagnostics) == 0, f"Expected 0 diagnostics after resolve, got {diagnostics}"  # keep-head


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


SNAPSHOT_URI = "file:///fake/snapshot_resolve_test.txt"


async def _open_snapshot_and_get_actions(client: LanguageClient, version: int = 1):
    client.text_document_did_open(
        DidOpenTextDocumentParams(
            text_document=TextDocumentItem(
                uri=SNAPSHOT_URI,
                language_id="text",
                version=version,
                text=CONFLICT_JJ_SNAPSHOT,
            )
        )
    )
    await client.wait_for_notification("textDocument/publishDiagnostics")
    diagnostics = client.diagnostics.get(SNAPSHOT_URI, [])
    assert len(diagnostics) == 1, f"Expected 1 diagnostic before resolution, got {len(diagnostics)}"

    actions = await asyncio.wait_for(
        asyncio.wrap_future(client.text_document_code_action(
            CodeActionParams(
                text_document=TextDocumentIdentifier(uri=SNAPSHOT_URI),
                range=Range(
                    start=Position(line=0, character=0),
                    end=Position(line=0, character=1),
                ),
                context=CodeActionContext(diagnostics=diagnostics),
            )
        )),
        timeout=3,
    )
    assert actions is not None and len(actions) > 0, "Expected at least one code action"
    return actions


async def _apply_snapshot_action_and_assert_clear(client: LanguageClient, action, version: int = 2):
    assert action.edit is not None
    changes = action.edit.changes
    assert changes is not None
    edits = changes[SNAPSHOT_URI]
    assert len(edits) == 1
    edit = edits[0]

    client.text_document_did_change(
        DidChangeTextDocumentParams(
            text_document=VersionedTextDocumentIdentifier(
                uri=SNAPSHOT_URI,
                version=version,
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

    diagnostics = client.diagnostics.get(SNAPSHOT_URI, [])
    assert len(diagnostics) == 0, f"Expected 0 diagnostics after resolve, got {diagnostics}"
    return edit


async def test_snapshot_resolve_keep_side_a_clears_diagnostics_and_inserts_content(client: LanguageClient):
    """Applying 'Keep side-A' on a jj snapshot conflict replaces the block with
    side-A content and clears the diagnostic."""
    actions = await _open_snapshot_and_get_actions(client)
    action = next(
        (a for a in actions if a.title == 'Keep \'abc12345 "first change"\''), None
    )
    assert action is not None, f"'Keep side-A' action not found; titles: {[a.title for a in actions]}"
    edit = await _apply_snapshot_action_and_assert_clear(client, action)
    assert edit.new_text == "apple\ngrapefruit\n", (
        f"Expected side-A content, got {edit.new_text!r}"
    )


async def test_snapshot_resolve_keep_side_b_clears_diagnostics_and_inserts_content(client: LanguageClient):
    """Applying 'Keep side-B' on a jj snapshot conflict replaces the block with
    side-B content and clears the diagnostic."""
    actions = await _open_snapshot_and_get_actions(client)
    action = next(
        (a for a in actions if a.title == 'Keep \'def67890 "second change"\''), None
    )
    assert action is not None, f"'Keep side-B' action not found; titles: {[a.title for a in actions]}"
    edit = await _apply_snapshot_action_and_assert_clear(client, action)
    assert edit.new_text == "APPLE\nGRAPE\n", (
        f"Expected side-B content, got {edit.new_text!r}"
    )


async def test_snapshot_resolve_keep_base_clears_diagnostics_and_inserts_content(client: LanguageClient):
    """Applying 'Keep base' on a jj snapshot conflict replaces the block with
    base content and clears the diagnostic."""
    actions = await _open_snapshot_and_get_actions(client)
    action = next(
        (a for a in actions if a.title == 'Keep \'base11111 "merge base"\''), None
    )
    assert action is not None, f"'Keep base' action not found; titles: {[a.title for a in actions]}"
    edit = await _apply_snapshot_action_and_assert_clear(client, action)
    assert edit.new_text == "apple\ngrape\n", (
        f"Expected base content, got {edit.new_text!r}"
    )


async def test_snapshot_resolve_keep_all_sides_clears_diagnostics_and_concatenates(client: LanguageClient):
    """Applying 'Keep all sides' on a jj snapshot conflict concatenates all side
    content with empty join (no separator) and clears the diagnostic."""
    actions = await _open_snapshot_and_get_actions(client)
    action = next((a for a in actions if a.title == "Keep all sides"), None)
    assert action is not None, f"'Keep all sides' action not found; titles: {[a.title for a in actions]}"
    edit = await _apply_snapshot_action_and_assert_clear(client, action)
    assert edit.new_text == "apple\ngrapefruit\nAPPLE\nGRAPE\n", (
        f"Expected concatenated sides, got {edit.new_text!r}"
    )


async def test_snapshot_resolve_drop_all_clears_diagnostics_and_removes_block(client: LanguageClient):
    """Applying 'Drop all' on a jj snapshot conflict replaces the entire block
    with an empty string and clears the diagnostic."""
    actions = await _open_snapshot_and_get_actions(client)
    action = next((a for a in actions if a.title == "Drop all"), None)
    assert action is not None, f"'Drop all' action not found; titles: {[a.title for a in actions]}"
    edit = await _apply_snapshot_action_and_assert_clear(client, action)
    assert edit.new_text == "", f"Expected empty string, got {edit.new_text!r}"


SNAPSHOT_3WAY_URI = "file:///fake/snapshot_3way_resolve_test.txt"


async def _open_3way_snapshot_and_get_actions(client: LanguageClient, version: int = 1):
    client.text_document_did_open(
        DidOpenTextDocumentParams(
            text_document=TextDocumentItem(
                uri=SNAPSHOT_3WAY_URI,
                language_id="text",
                version=version,
                text=CONFLICT_JJ_SNAPSHOT_3WAY,
            )
        )
    )
    await client.wait_for_notification("textDocument/publishDiagnostics")
    diagnostics = client.diagnostics.get(SNAPSHOT_3WAY_URI, [])
    assert len(diagnostics) == 1, f"Expected 1 diagnostic before resolution, got {len(diagnostics)}"

    actions = await asyncio.wait_for(
        asyncio.wrap_future(client.text_document_code_action(
            CodeActionParams(
                text_document=TextDocumentIdentifier(uri=SNAPSHOT_3WAY_URI),
                range=Range(
                    start=Position(line=0, character=0),
                    end=Position(line=0, character=1),
                ),
                context=CodeActionContext(diagnostics=diagnostics),
            )
        )),
        timeout=3,
    )
    assert actions is not None, "Expected a non-null action list"
    return actions


async def _apply_3way_snapshot_action_and_assert_clear(
    client: LanguageClient, action, version: int = 2,
):
    assert action.edit is not None
    changes = action.edit.changes
    assert changes is not None
    edits = changes[SNAPSHOT_3WAY_URI]
    assert len(edits) == 1
    edit = edits[0]

    client.text_document_did_change(
        DidChangeTextDocumentParams(
            text_document=VersionedTextDocumentIdentifier(
                uri=SNAPSHOT_3WAY_URI,
                version=version,
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

    diagnostics = client.diagnostics.get(SNAPSHOT_3WAY_URI, [])
    assert len(diagnostics) == 0, f"Expected 0 diagnostics after resolve, got {diagnostics}"
    return edit


async def test_3way_snapshot_offers_7_actions_with_expected_titles(client: LanguageClient):
    """An N=3 octopus jj snapshot conflict offers 3 Keep-side + 2 Keep-base +
    Keep all sides + Drop all = 7 actions."""
    actions = await _open_3way_snapshot_and_get_actions(client)
    titles = sorted(a.title for a in actions)
    expected = sorted([
        'Keep \'cid0 1234abcd "change 0"\'',
        'Keep \'cid1 5678efgh "change 1"\'',
        'Keep \'cid2 9abcijkl "change 2"\'',
        'Keep \'bid0 aaaa0000 "base 0"\'',
        'Keep \'bid1 bbbb0000 "base 1"\'',
        "Keep all sides",
        "Drop all",
    ])
    assert titles == expected, f"Unexpected titles:\n  got: {titles}\n  want: {expected}"


async def test_3way_snapshot_keep_middle_side_replaces_with_that_content(client: LanguageClient):
    """Applying a middle-side Keep on a 3-way conflict replaces the whole block
    with only that side's content."""
    actions = await _open_3way_snapshot_and_get_actions(client)
    action = next(
        (a for a in actions if a.title == 'Keep \'cid1 5678efgh "change 1"\''), None
    )
    assert action is not None, f"middle-side action missing; titles: {[a.title for a in actions]}"
    edit = await _apply_3way_snapshot_action_and_assert_clear(client, action)
    assert edit.new_text == "beta\n", f"Expected middle-side content, got {edit.new_text!r}"


async def test_3way_snapshot_keep_second_base_replaces_with_that_base_content(client: LanguageClient):
    """The second of the two interleaved bases is independently selectable as
    its own code action; applying it replaces the block with that base's content."""
    actions = await _open_3way_snapshot_and_get_actions(client)
    action = next(
        (a for a in actions if a.title == 'Keep \'bid1 bbbb0000 "base 1"\''), None
    )
    assert action is not None, f"second-base action missing; titles: {[a.title for a in actions]}"
    edit = await _apply_3way_snapshot_action_and_assert_clear(client, action)
    assert edit.new_text == "one\n", f"Expected second-base content, got {edit.new_text!r}"


async def test_3way_snapshot_keep_all_sides_concatenates_all_three(client: LanguageClient):
    """Keep all sides on a 3-way conflict concatenates all three side contents
    in document order with empty join; both bases are omitted."""
    actions = await _open_3way_snapshot_and_get_actions(client)
    action = next((a for a in actions if a.title == "Keep all sides"), None)
    assert action is not None
    edit = await _apply_3way_snapshot_action_and_assert_clear(client, action)
    assert edit.new_text == "alpha\nbeta\ngamma\n", (
        f"Expected all-sides concatenation, got {edit.new_text!r}"
    )
