"""Concurrency/thread-strain tests for the LSP server.

The server spawns one worker thread per document update (didOpen, didChange)
and holds its state (`documents` map + per-document DocumentState) behind
`parking_lot::Mutex` guards. These tests burst operations at the server to
force contention on both locks and exercise the version-skew guard in
`on_document_update`.

Success criteria for every test: the server stays alive, never deadlocks,
and converges on the correct final diagnostic state.
"""

import asyncio

from lsprotocol.types import (
    CodeActionContext,
    CodeActionParams,
    DidChangeTextDocumentParams,
    DidCloseTextDocumentParams,
    DidOpenTextDocumentParams,
    Position,
    Range,
    TextDocumentContentChangeWholeDocument,
    TextDocumentIdentifier,
    TextDocumentItem,
    VersionedTextDocumentIdentifier,
)
from pytest_lsp import LanguageClient

from conftest import CONFLICT_DIFF3, CONFLICT_SIMPLE, PLAIN_TEXT


async def _wait_for_all_diagnostics(
    client: LanguageClient, uris: list[str], timeout: float = 10.0
) -> list[str]:
    """Poll until every URI in `uris` has an entry in client.diagnostics.

    Returns any URIs that never reported. pytest-lsp stores the latest
    publishDiagnostics payload per URI in `client.diagnostics`, so presence
    of the key means the server has published for that document at least
    once.
    """
    loop = asyncio.get_event_loop()
    deadline = loop.time() + timeout
    while loop.time() < deadline:
        missing = [u for u in uris if u not in client.diagnostics]
        if not missing:
            return []
        await asyncio.sleep(0.02)
    return [u for u in uris if u not in client.diagnostics]


async def _wait_until(
    predicate, timeout: float = 10.0, interval: float = 0.02
) -> bool:
    loop = asyncio.get_event_loop()
    deadline = loop.time() + timeout
    while loop.time() < deadline:
        if predicate():
            return True
        await asyncio.sleep(interval)
    return predicate()


async def test_many_documents_opened_concurrently(client: LanguageClient):
    """Burst-open 25 documents; each spawns a worker thread and every one
    must publish the right diagnostic count.

    Stresses the outer `documents` mutex (all workers call `state.documents.lock()`
    at roughly the same time) and confirms no worker gets starved or
    delivers diagnostics for the wrong URI.
    """
    n = 25
    uris = [f"file:///fake/concurrent_doc_{i}.txt" for i in range(n)]

    for i, uri in enumerate(uris):
        text = CONFLICT_SIMPLE if i % 2 == 0 else CONFLICT_DIFF3
        client.text_document_did_open(
            DidOpenTextDocumentParams(
                text_document=TextDocumentItem(
                    uri=uri,
                    language_id="text",
                    version=1,
                    text=text,
                )
            )
        )

    missing = await _wait_for_all_diagnostics(client, uris, timeout=10.0)
    assert not missing, f"Never received diagnostics for: {missing}"

    for uri in uris:
        diags = client.diagnostics.get(uri, [])
        assert len(diags) == 1, f"{uri}: expected 1 diagnostic, got {diags}"


async def test_rapid_successive_changes_to_one_document(client: LanguageClient):
    """Fire a burst of whole-document replacements; server must converge on
    the final version.

    The server applies changes sequentially on the main thread but spawns a
    parse worker per change. Stale workers (version < current doc version)
    are filtered by the guard in `on_document_update`. The final publish
    must reflect the last-applied version.
    """
    uri = "file:///fake/rapid_changes.txt"

    client.text_document_did_open(
        DidOpenTextDocumentParams(
            text_document=TextDocumentItem(
                uri=uri,
                language_id="text",
                version=1,
                text=PLAIN_TEXT,
            )
        )
    )
    await _wait_for_all_diagnostics(client, [uri], timeout=5.0)

    n = 40
    # Versions 2..=41. Even versions = conflict, odd = plain.
    # Final version (41) is odd → final state should be no diagnostics.
    for version in range(2, 2 + n):
        text = CONFLICT_SIMPLE if version % 2 == 0 else PLAIN_TEXT
        client.text_document_did_change(
            DidChangeTextDocumentParams(
                text_document=VersionedTextDocumentIdentifier(
                    uri=uri, version=version
                ),
                content_changes=[TextDocumentContentChangeWholeDocument(text=text)],
            )
        )

    settled = await _wait_until(
        lambda: len(client.diagnostics.get(uri, [])) == 0,
        timeout=10.0,
    )
    diags = client.diagnostics.get(uri, [])
    assert settled, f"Server never settled on final plain state; last diags={diags}"

    # Liveness: one more cycle should behave normally.
    client.text_document_did_change(
        DidChangeTextDocumentParams(
            text_document=VersionedTextDocumentIdentifier(uri=uri, version=100),
            content_changes=[TextDocumentContentChangeWholeDocument(text=CONFLICT_SIMPLE)],
        )
    )
    settled = await _wait_until(
        lambda: len(client.diagnostics.get(uri, [])) == 1,
        timeout=5.0,
    )
    assert settled, f"After rapid churn, server did not process new change; diags={client.diagnostics.get(uri, [])}"


async def test_open_close_reopen_churn(client: LanguageClient):
    """Repeatedly open/close/re-open the same URI with interleaved changes.

    Exercises the race between didClose (removes entry from the map) and
    in-flight worker threads that may still hold an Arc-clone of the doc's
    Mutex. Worker threads that find the doc missing must bail cleanly; the
    ones that have already cloned the Arc must not corrupt anything when
    their update races with removal.
    """
    uri = "file:///fake/churn.txt"
    rounds = 15

    for round_idx in range(rounds):
        client.text_document_did_open(
            DidOpenTextDocumentParams(
                text_document=TextDocumentItem(
                    uri=uri,
                    language_id="text",
                    version=1,
                    text=CONFLICT_SIMPLE,
                )
            )
        )
        client.text_document_did_change(
            DidChangeTextDocumentParams(
                text_document=VersionedTextDocumentIdentifier(uri=uri, version=2),
                content_changes=[
                    TextDocumentContentChangeWholeDocument(text=CONFLICT_DIFF3),
                ],
            )
        )
        client.text_document_did_close(
            DidCloseTextDocumentParams(
                text_document=TextDocumentIdentifier(uri=uri),
            )
        )

    # Final reopen to verify the server is still responsive and correct.
    final_uri = "file:///fake/churn_final.txt"
    client.text_document_did_open(
        DidOpenTextDocumentParams(
            text_document=TextDocumentItem(
                uri=final_uri,
                language_id="text",
                version=1,
                text=CONFLICT_SIMPLE,
            )
        )
    )
    settled = await _wait_until(
        lambda: len(client.diagnostics.get(final_uri, [])) == 1,
        timeout=5.0,
    )
    assert settled, (
        f"Server unresponsive after {rounds} open/close cycles; "
        f"diags={client.diagnostics.get(final_uri, [])}"
    )


async def test_code_actions_during_rapid_changes(client: LanguageClient):
    """Request code actions while a burst of changes is in flight.

    This is the main reader+writer test for the outer `documents` mutex:
    `code_action` is a request handled synchronously on the main loop
    thread, while worker threads for in-flight changes lock the same
    mutex. parking_lot's `Mutex` must hand out the lock fairly and never
    deadlock.
    """
    uri = "file:///fake/ca_during_churn.txt"

    client.text_document_did_open(
        DidOpenTextDocumentParams(
            text_document=TextDocumentItem(
                uri=uri,
                language_id="text",
                version=1,
                text=CONFLICT_SIMPLE,
            )
        )
    )
    await _wait_until(
        lambda: len(client.diagnostics.get(uri, [])) == 1,
        timeout=5.0,
    )

    # Fire 20 alternating didChanges.
    for version in range(2, 22):
        text = CONFLICT_SIMPLE if version % 2 == 0 else CONFLICT_DIFF3
        client.text_document_did_change(
            DidChangeTextDocumentParams(
                text_document=VersionedTextDocumentIdentifier(uri=uri, version=version),
                content_changes=[TextDocumentContentChangeWholeDocument(text=text)],
            )
        )

    # Fire several code-action requests concurrently; each returns a Future.
    # All must complete without timing out.
    futures = [
        asyncio.wrap_future(
            client.text_document_code_action(
                CodeActionParams(
                    text_document=TextDocumentIdentifier(uri=uri),
                    range=Range(
                        start=Position(line=1, character=0),
                        end=Position(line=1, character=1),
                    ),
                    context=CodeActionContext(diagnostics=[]),
                )
            )
        )
        for _ in range(8)
    ]

    results = await asyncio.wait_for(asyncio.gather(*futures), timeout=10.0)
    # Any response is fine — possibly empty if the request arrived between
    # a conflict-clearing change and its worker. The point is no hang, no crash.
    assert all(r is not None for r in results), "Some code-action requests returned None"

    # Liveness: server still handles a fresh request afterwards.
    client.text_document_did_change(
        DidChangeTextDocumentParams(
            text_document=VersionedTextDocumentIdentifier(uri=uri, version=200),
            content_changes=[TextDocumentContentChangeWholeDocument(text=CONFLICT_SIMPLE)],
        )
    )
    settled = await _wait_until(
        lambda: len(client.diagnostics.get(uri, [])) == 1,
        timeout=5.0,
    )
    assert settled, "Server unresponsive after code-actions-during-churn"
