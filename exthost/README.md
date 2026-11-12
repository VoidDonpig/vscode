# code-exthost

Runs VS Code's real Node extension host from Rust and plays the workbench
("main thread") side of its RPC protocol. This is the extension layer for a
native, Electron-free UI: extensions stay unmodified JavaScript; everything
they talk to is answered from Rust.

- Framing: `PersistentProtocol` (`src/vs/base/parts/ipc/common/ipc.net.ts`)
- Messages: `rpcProtocol.ts` `MessageIO`; proxy ids generated from `extHost.protocol.ts` by `build.rs`
- Requests from the extension host go to a `Handler` (`MainThread*` side); unknown calls reply `undefined`.

```bash
npm run compile          # builds out/ (extension host JS)
cd exthost && cargo test # tests/hello_extension.rs runs fixtures/hello-ext: command + completions
```

Status: handshake, commands, documents, completions. Not yet: other language
features, configuration values, file system, webviews, terminals, Windows (named pipe).
