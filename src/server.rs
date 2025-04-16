type LSPResult = anyhow::Result<Option<lsp_server::Notification>>;

pub struct MergeAssistant {
    connection: lsp_server::Connection,
}

impl MergeAssistant {
    pub fn main_loop(connection: lsp_server::Connection) -> LSPResult {
        let mut server = MergeAssistant { connection };
        server.real_main_loop()?;
        log::info!("shutting down server");
        Ok(None)
    }

    fn real_main_loop(&mut self) -> LSPResult {
        for msg in &self.connection.receiver {
            log::debug!("got msg: {msg:?}");
            match msg {
                lsp_server::Message::Notification(notification) => {
                    if let Some(reply) = self.on_notification_message(notification)? {
                        _ = self.connection.sender.send(reply.into());
                    }
                }
                lsp_server::Message::Request(req) => {
                    if self.connection.handle_shutdown(&req)? {
                        return Ok(None);
                    }
                    self.on_new_request(req)?;
                }
                lsp_server::Message::Response(resp) => {
                    log::debug!("got response: {resp:?}");
                }
            }
        }
        Ok(None)
    }

    pub fn server_capabilities() -> lsp_types::ServerCapabilities {
        let text_document_sync = Some(lsp_types::TextDocumentSyncCapability::Kind(
            lsp_types::TextDocumentSyncKind::FULL,
        ));
        let code_action_provider = Some(lsp_types::CodeActionProviderCapability::Options(
            lsp_types::CodeActionOptions {
                code_action_kinds: Some(vec![lsp_types::CodeActionKind::QUICKFIX]),
                ..Default::default()
            },
        ));
        lsp_types::ServerCapabilities {
            text_document_sync,
            code_action_provider,
            ..Default::default()
        }
    }

    fn on_did_open_text_document(&self, notification: lsp_server::Notification) -> LSPResult {
        log::debug!("did open intro");
        let params: lsp_types::DidOpenTextDocumentParams =
            serde_json::from_value(notification.params)?;
        log::debug!(
            "did open: {:?}: {:?}",
            params.text_document.uri,
            params.text_document.text
        );
        let mut parser = crate::parser::Parser::default();
        let conflicts = parser.parse(&params.text_document);
        log::debug!("Conflicts: {conflicts:?}");
        let diagnostics: Vec<lsp_types::Diagnostic> =
            conflicts.iter().map(lsp_types::Diagnostic::from).collect();
        let publish_diagnostics_params = lsp_types::PublishDiagnosticsParams {
            uri: params.text_document.uri,
            diagnostics,
            version: Some(params.text_document.version),
        };
        let notification = lsp_server::Notification::new(
        <lsp_types::notification::PublishDiagnostics as lsp_types::notification::Notification>::METHOD.to_owned(),
        publish_diagnostics_params,
    );

        Ok(Some(notification))
    }

    fn on_did_change_text_document(&self, notification: lsp_server::Notification) -> LSPResult {
        let params: lsp_types::DidChangeTextDocumentParams =
            serde_json::from_value(notification.params)?;
        log::debug!(
            "did change: {:?}: {:?}",
            params.text_document.uri,
            params.content_changes
        );
        Ok(None)
    }

    fn on_did_close_text_document(&self, notification: lsp_server::Notification) -> LSPResult {
        let params: lsp_types::DidCloseTextDocumentParams =
            serde_json::from_value(notification.params)?;
        log::debug!("did close: {:?}", params.text_document.uri);
        Ok(None)
    }

    fn on_notification_message(&self, notification: lsp_server::Notification) -> LSPResult {
        log::debug!("heard notification {notification:?}");
        match notification.method.as_ref() {
            "textDocument/didOpen" => self.on_did_open_text_document(notification),
            "textDocument/didClose" => self.on_did_close_text_document(notification),
            "textDocument/didChange" => self.on_did_change_text_document(notification),
            unknown => {
                log::debug!("notification: unknown: {unknown:?}");
                Ok(None)
            }
        }
    }

    fn on_new_request(&self, req: lsp_server::Request) -> LSPResult {
        log::debug!("got request: {req:?}");

        Ok(None)
    }
}
