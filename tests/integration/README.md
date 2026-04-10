# Testing

This harness is based on pytest-lsp. It will run merge-conflict-assistant and communicate with it like any other
editor. There are some hopefully easy follow examples when writing new ones.

## Running

```
python -m venv venv
source venv/bin/activate  # or the right one for your shell
pip install .
pytest -v
```
