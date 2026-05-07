# Changelog

Format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
The project follows [Semantic Versioning](https://semver.org/).

## [0.8.0] - 2026-05-06

### Added
- Bulk diff3 code actions: when a file contains two or more conflicts, two extra
  file-wide actions are offered alongside the per-site ones — `Keep HEAD in
  remaining conflicts` and `Keep branch-name in remaining conflicts`. They apply
  the corresponding choice to every diff3 conflict still in the file in a single
  atomic edit, supporting a rebase workflow where you walk diagnostics, resolve
  the tricky ones by hand, and clean up the rest in one shot.

## [0.7.0] - 2026-04-21

### Added
- Support for Jujutsu (jj) snapshot conflicts. The parser now recognizes two
  marker families — diff3-style and jj snapshot (`+++++++` sides, `-------`
  base) — and discriminates between them based on the first inner marker after
  `<<<<<<<`. A file mixing both families returns `ParseError::MixedFormat`.

  Supporting other formats should be possible but has not been explored.

## [0.6.1] - 2026-04-21

### Changed
- Switched internal locking from `std::sync::Mutex` to `parking_lot::Mutex`.

## [0.6.0] - 2026-04-13

### Changed
- Replaced the hand-rolled document handling with the `lsp-textdocument` crate.
- Major internal refactor; free functions moved into `DocumentState`.

### Added
- Nix flake and home-manager module (thanks @andrewvious, #1).

## [0.5.2] - 2026-04-10

### Added
- `--log` CLI flag for writing logs to a file, with handling so concurrent
  editor sessions don't clobber each other's log output.
- Tool for generating conflict files for testing and development.

### Changed
- Logging refresh: routed through the LSP `logMessage` channel.
- Documentation refresh.

## [0.5.0] - 2026-04-10

First public release. Highlights:

### Added
- Detection of standard two-way and diff3 conflict markers.
- Code actions: `Keep HEAD`, `Keep branch`, `Keep ancestor` (diff3), `Keep
  both`, and `Drop all`.
- Per-document locking so concurrent updates to different documents don't
  contend on a single mutex.
- Marker normalization and ascending-range validation in the parser.
- Proper LSP-spec response when a code action request lands outside any
  conflict (empty list rather than error).
- Cleaner shutdown and exit message handling.
- Integration test suite under `tests/integration/`, driving the compiled
  binary over stdio with `pytest-lsp`.

[0.8.0]: https://github.com/shaleh/merge-conflict-assistant/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/shaleh/merge-conflict-assistant/compare/v0.6.1...v0.7.0
[0.6.1]: https://github.com/shaleh/merge-conflict-assistant/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/shaleh/merge-conflict-assistant/compare/v0.5.2...v0.6.0
[0.5.2]: https://github.com/shaleh/merge-conflict-assistant/compare/v0.5.0...v0.5.2
[0.5.0]: https://github.com/shaleh/merge-conflict-assistant/releases/tag/v0.5.0
