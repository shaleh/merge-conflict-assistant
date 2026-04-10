"""Tests for the --log CLI option."""

import pathlib
import shutil
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

# Use a temporary directory so we can glob for the PID-stamped log file.
_log_dir = tempfile.mkdtemp(prefix="merge-conflict-assistant-test-")
LOG_DIR = pathlib.Path(_log_dir)
LOG_BASE = LOG_DIR / "merge-conflict-assistant.log"

TEST_URI = "file:///fake/log_test.txt"


@pytest_lsp.fixture(
    config=ClientServerConfig(
        server_command=[str(SERVER_BIN), "--debug", "--log", str(LOG_BASE)],
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
    """When --log is provided, the server writes tracing output to a PID-stamped file."""
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

    # The server stamps the log filename with its PID: merge-conflict-assistant-<pid>.log
    log_files = list(LOG_DIR.glob("merge-conflict-assistant-*.log"))
    assert len(log_files) == 1, f"Expected one log file, found: {log_files}"
    contents = log_files[0].read_text()
    assert len(contents) > 0, "Log file should not be empty"
    assert "server initializing" in contents


@pytest.fixture(autouse=True, scope="module")
def cleanup_log_dir():
    yield

    shutil.rmtree(LOG_DIR, ignore_errors=True)
