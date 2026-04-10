"""Tests for the --log CLI option."""

import os
import pathlib
import tempfile

import pytest
import pytest_lsp
from lsprotocol.types import (
    ClientCapabilities,
    DidOpenTextDocumentParams,
    InitializeParams,
    PublishDiagnosticsClientCapabilities,
    TextDocumentClientCapabilities,
    TextDocumentItem,
)
from pytest_lsp import ClientServerConfig, LanguageClient

from conftest import CONFLICT_SIMPLE, SERVER_BIN

# Generate a unique temporary log file path for this test module.
_log_fd, _log_path_str = tempfile.mkstemp(suffix=".log", prefix="mca-test-")
os.close(_log_fd)
LOG_FILE = pathlib.Path(_log_path_str)

TEST_URI = "file:///fake/log_test.txt"


@pytest_lsp.fixture(
    config=ClientServerConfig(
        server_command=[str(SERVER_BIN), "--debug", "--log", str(LOG_FILE)],
    ),
)
async def log_client(lsp_client: LanguageClient):
    params = InitializeParams(
        capabilities=ClientCapabilities(
            text_document=TextDocumentClientCapabilities(
                publish_diagnostics=PublishDiagnosticsClientCapabilities(),
            ),
        ),
    )
    await lsp_client.initialize_session(params)
    yield lsp_client
    await lsp_client.shutdown_session()


async def test_log_file_has_server_output(log_client: LanguageClient):
    """When --log is provided, the server writes tracing output to the file."""
    log_client.text_document_did_open(
        DidOpenTextDocumentParams(
            text_document=TextDocumentItem(
                uri=TEST_URI,
                language_id="text",
                version=1,
                text=CONFLICT_SIMPLE,
            )
        )
    )

    await log_client.wait_for_notification("textDocument/publishDiagnostics")

    contents = LOG_FILE.read_text()
    assert len(contents) > 0, "Log file should not be empty"
    assert "server initializing" in contents


@pytest.fixture(autouse=True, scope="module")
def cleanup_log_file():
    yield
    LOG_FILE.unlink(missing_ok=True)
