# ChocoFactory

## Manual smoke testing

`chokofactoryd` spawns the real `claude` CLI by default — running the
daemon manually will hit the real, billable `claude` unless you point it
at a stand-in first:

```
cargo build -p mock-claude
CHOKOFACTORY_CLAUDE_BINARY=$(pwd)/target/debug/mock-claude cargo run -p chokofactoryd
```

`mock-claude` echoes back whatever it's sent (`echo:{text}`); set
`MOCK_CLAUDE_REPLY=<text>` to get a fixed reply instead. Point
`CHOKOFACTORY_CLAUDE_BINARY` at the real `claude` binary only when you
specifically mean to exercise the real CLI.