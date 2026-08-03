# Bring Your Own Bot

Bring Your Own Bot is MarketState's local model connector. Its first provider is Codex: the small macOS and Windows app lets Orama use the Codex subscription already authenticated on that computer. It listens only on `127.0.0.1:8765`; Codex credentials stay in Codex's local credential store.

> Connect the AI subscription you already use.

## User setup

1. Download and install the connector for your operating system.
2. Open it and choose **Connect Codex**. Complete OpenAI's browser sign-in.
3. Leave **Start automatically** enabled.
4. Return to Orama and choose **Retry**. The first verified request links the connector to that MarketState user.

Changing the linked MarketState user requires the explicit **Unlink user** action in the connector. That action also signs out of Codex, so the next user must authenticate their own Codex account. A different signed-in MarketState user receives `403 Forbidden`.

## Local development

Codex CLI and Rust must already be installed. The staging script copies the native Codex executable—not its Node launcher—into the Tauri bundle.

```sh
npm run tauri:dev
npm run tauri:build
```

The browser-facing endpoints are `/health`, `/chat`, and `/cancel`. Each requires a valid MarketState Supabase bearer token. The connector validates the token with MarketState's Supabase Auth service and persists only the verified user UUID and email in `owner.json`.

## Security boundary

- The server binds only to loopback and accepts only approved MarketState and local development origins.
- Every protected request is authenticated; client-supplied user IDs are ignored.
- The first verified user claims the installation. Subsequent identities are rejected until an explicit unlink, which also clears the previous Codex login.
- Codex credentials are read only by the bundled Codex runtime under the current operating-system user.
- Separate operating-system accounts are required for a genuinely isolated shared computer.
