"""Test with Helix editor's exact initialize capabilities.

Helix sends a richer set of capabilities than the minimal set in our
other tests.  This reproduces the environment where the server was
observed to close the stream before responding to initialize.
"""

import pytest_lsp
from lsprotocol.types import (
    ClientCapabilities,
    ClientInfo,
    DidOpenTextDocumentParams,
    GeneralClientCapabilities,
    InitializeParams,
    PublishDiagnosticsClientCapabilities,
    TextDocumentClientCapabilities,
    TextDocumentItem,
    TextDocumentSyncClientCapabilities,
    WindowClientCapabilities,
    WorkspaceClientCapabilities,
    WorkspaceFolder,
)
from pytest_lsp import ClientServerConfig, LanguageClient

from conftest import CONFLICT_SIMPLE, SERVER_BIN

TEST_URI = "file:///fake/helix_test.txt"


def _helix_capabilities() -> ClientCapabilities:
    """Return capabilities that closely match what Helix 25.07.1 sends."""
    return ClientCapabilities(
        general=GeneralClientCapabilities(
            position_encodings=["utf-8", "utf-32", "utf-16"],
        ),
        text_document=TextDocumentClientCapabilities(
            publish_diagnostics=PublishDiagnosticsClientCapabilities(
                version_support=True,
            ),
            synchronization=TextDocumentSyncClientCapabilities(
                dynamic_registration=False,
            ),
        ),
        window=WindowClientCapabilities(
            work_done_progress=True,
        ),
        workspace=WorkspaceClientCapabilities(
            apply_edit=True,
            configuration=True,
            workspace_folders=True,
        ),
    )


@pytest_lsp.fixture(
    config=ClientServerConfig(server_command=[str(SERVER_BIN), "--debug"]),
)
async def helix_client(lsp_client: LanguageClient):
    params = InitializeParams(
        capabilities=_helix_capabilities(),
        client_info=ClientInfo(
            name="helix",
            version="25.07.1",
        ),
        root_uri="file:///fake/workspace",
        workspace_folders=[
            WorkspaceFolder(name="workspace", uri="file:///fake/workspace"),
        ],
    )
    result = await lsp_client.initialize_session(params)
    lsp_client.init_result = result
    yield lsp_client
    await lsp_client.shutdown_session()


async def test_helix_initialize(helix_client: LanguageClient):
    """Server survives Helix's capabilities and returns a valid response."""
    caps = helix_client.init_result.capabilities
    assert caps.text_document_sync is not None
    assert caps.code_action_provider is not None


async def test_helix_did_open_with_conflicts(helix_client: LanguageClient):
    """Server publishes diagnostics through a Helix-like session."""
    helix_client.text_document_did_open(
        DidOpenTextDocumentParams(
            text_document=TextDocumentItem(
                uri=TEST_URI,
                language_id="text",
                version=1,
                text=CONFLICT_SIMPLE,
            )
        )
    )

    await helix_client.wait_for_notification("textDocument/publishDiagnostics")

    diagnostics = helix_client.diagnostics.get(TEST_URI, [])
    assert len(diagnostics) > 0
