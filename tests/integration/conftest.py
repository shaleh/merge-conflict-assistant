import pathlib

import pytest_lsp
from lsprotocol.types import (
    ClientCapabilities,
    InitializeParams,
    TextDocumentClientCapabilities,
    PublishDiagnosticsClientCapabilities,
)
from pytest_lsp import ClientServerConfig, LanguageClient

SERVER_BIN = (
    pathlib.Path(__file__).resolve().parent.parent.parent
    / "target"
    / "debug"
    / "merge-conflict-assistant"
)


@pytest_lsp.fixture(
    config=ClientServerConfig(server_command=[str(SERVER_BIN), "--debug"]),
)
async def client(lsp_client: LanguageClient):
    params = InitializeParams(
        capabilities=ClientCapabilities(
            text_document=TextDocumentClientCapabilities(
                publish_diagnostics=PublishDiagnosticsClientCapabilities(),
            ),
        ),
    )
    result = await lsp_client.initialize_session(params)
    lsp_client.init_result = result
    yield lsp_client
    await lsp_client.shutdown_session()


# Conflict text helpers — unlike the Rust source, Python test files are not
# parsed by the merge-conflict detector so literal markers are safe here.

CONFLICT_SIMPLE = """\
<<<<<<< HEAD
head content
=======
branch content
>>>>>>> branch
"""

CONFLICT_DIFF3 = """\
<<<<<<< HEAD
head content
||||||| ancestor
original content
=======
branch content
>>>>>>> branch
"""

PLAIN_TEXT = "hello world\nno conflicts here\n"
